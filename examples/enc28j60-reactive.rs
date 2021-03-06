//! This is the reactive version of the `enc28j60` example

#![deny(unsafe_code)]
#![deny(warnings)]
#![feature(lang_items)]
#![feature(nll)]
#![feature(proc_macro)]
#![no_std]

#[macro_use]
extern crate cortex_m;
extern crate cortex_m_rtfm as rtfm;
extern crate enc28j60;
extern crate heapless;
extern crate jnet;
extern crate stm32f103xx_hal as hal;

use cortex_m::peripheral::{DWT, ITM};
use enc28j60::{Enc28j60, Event};
use hal::delay::Delay;
use hal::gpio::gpioa::{PA0, PA3, PA4, PA5, PA6, PA7};
use hal::gpio::gpioc::PC13;
use hal::gpio::{Alternate, Floating, Input, Output, PushPull};
use hal::prelude::*;
use hal::spi::Spi;
use hal::stm32f103xx::{self, Interrupt, SPI1};
use hal::timer::{self, Timer};
use heapless::LinearMap;
use jnet::{arp, ether, icmp, mac, udp, Buffer, ipv4};
use rtfm::{app, Resource, Threshold};

// uncomment to disable tracing
// macro_rules! iprintln {
//     ($($tt: tt)*) => {};
// }

/* Constants */
const KB: u16 = 1024; // bytes

/* Network configuration */
const MAC: mac::Addr = mac::Addr([0x20, 0x18, 0x03, 0x01, 0x00, 0x00]);
const IP: ipv4::Addr = ipv4::Addr([192, 168, 1, 33]);

/* Hardware configuration */
type Spi1 = Spi<
    SPI1,
    (
        PA5<Alternate<PushPull>>,
        PA6<Input<Floating>>,
        PA7<Alternate<PushPull>>,
    ),
>;
type Ncs = PA4<Output<PushPull>>;
type Int = PA0<Input<Floating>>;
type Reset = PA3<Output<PushPull>>;
type Led = PC13<Output<PushPull>>;

app! {
    device: stm32f103xx,

    resources: {
        static ARP_CACHE: LinearMap<ipv4::Addr, mac::Addr, [(ipv4::Addr, mac::Addr); 8]> =
            LinearMap::new();
        static SLEEP: u32 = 0;

        static ENC28J60: Enc28j60<Spi1, Ncs, Int, Reset>;
        static EXTI: stm32f103xx::EXTI;
        static ITM: ITM;
        static LED: Led;
    },

    idle: {
        resources: [SLEEP],
    },

    tasks: {
        EXTI0: {
            path: exti0,
            resources: [ARP_CACHE, ENC28J60, EXTI, LED, ITM],
        },

        SYS_TICK: {
            path: sys_tick,
            resources: [SLEEP, ITM],
        },
    },
}

fn init(mut p: init::Peripherals, _r: init::Resources) -> init::LateResources {
    let mut rcc = p.device.RCC.constrain();
    let mut afio = p.device.AFIO.constrain(&mut rcc.apb2);
    let mut flash = p.device.FLASH.constrain();
    let mut gpioa = p.device.GPIOA.split(&mut rcc.apb2);

    let clocks = rcc.cfgr.freeze(&mut flash.acr);

    p.core.DWT.enable_cycle_counter();

    // LED
    let mut gpioc = p.device.GPIOC.split(&mut rcc.apb2);
    let mut led = gpioc.pc13.into_push_pull_output(&mut gpioc.crh);
    // turn the LED off during initialization
    led.set_high();

    // SPI
    let mut ncs = gpioa.pa4.into_push_pull_output(&mut gpioa.crl);
    ncs.set_high();
    let sck = gpioa.pa5.into_alternate_push_pull(&mut gpioa.crl);
    let miso = gpioa.pa6;
    let mosi = gpioa.pa7.into_alternate_push_pull(&mut gpioa.crl);
    let spi = Spi::spi1(
        p.device.SPI1,
        (sck, miso, mosi),
        &mut afio.mapr,
        enc28j60::MODE,
        1.mhz(),
        clocks,
        &mut rcc.apb2,
    );

    // ENC28J60
    let mut reset = gpioa.pa3.into_push_pull_output(&mut gpioa.crl);
    reset.set_high();
    let int = gpioa.pa0.into_floating_input(&mut gpioa.crl);
    // configure EXTI0 interrupt
    // FIXME turn this into a higher level API
    p.device.EXTI.imr.write(|w| w.mr0().set_bit()); // unmask the interrupt (EXTI)
    p.device.EXTI.ftsr.write(|w| w.tr0().set_bit()); // trigger interrupt on falling edge
    let mut delay = Delay::new(p.core.SYST, clocks);
    let mut enc28j60 = Enc28j60::new(spi, ncs, int, reset, &mut delay, 7 * KB, MAC.0).ok().unwrap();

    // LED on after initialization
    led.set_low();

    // FIXME some frames are lost when sent right after initialization
    delay.delay_ms(100_u8);

    enc28j60.listen(Event::Pkt).ok().unwrap();

    Timer::syst(delay.free(), 1.hz(), clocks).listen(timer::Event::Update);

    // there may be some packets pending to be processed
    rtfm::set_pending(Interrupt::EXTI0);

    init::LateResources {
        ENC28J60: enc28j60,
        EXTI: p.device.EXTI,
        ITM: p.core.ITM,
        LED: led,
    }
}

fn idle(t: &mut Threshold, mut r: idle::Resources) -> ! {
    loop {
        rtfm::atomic(t, |t| {
            let before = DWT::get_cycle_count();
            rtfm::wfi();
            let after = DWT::get_cycle_count();

            *r.SLEEP.borrow_mut(t) += after.wrapping_sub(before);
        });

        // interrupts are serviced here
    }
}

fn exti0(_t: &mut Threshold, mut r: EXTI0::Resources) {
    let mut cache = r.ARP_CACHE;
    let mut enc28j60 = r.ENC28J60;
    let mut led = r.LED;
    let _stim = &mut r.ITM.stim[0];

    let mut buf = [0; 256];
    while enc28j60.interrupt_pending() {
        let mut buf = Buffer::new(&mut buf);
        let len = enc28j60.receive(buf.as_mut()).ok().unwrap();
        buf.truncate(len);

        if let Ok(mut eth) = ether::Frame::parse(buf) {
            iprintln!(_stim, "\nRx({})", eth.as_bytes().len());
            iprintln!(_stim, "* {:?}", eth);

            let src_mac = eth.get_source();

            match eth.get_type() {
                ether::Type::Arp => {
                    if let Ok(arp) = arp::Packet::parse(eth.payload_mut()) {
                        match arp.downcast() {
                            Ok(mut arp) => {
                                iprintln!(_stim, "** {:?}", arp);

                                if !arp.is_a_probe() {
                                    cache.insert(arp.get_spa(), arp.get_sha()).ok();
                                }

                                // are they asking for us?
                                if arp.get_oper() == arp::Operation::Request && arp.get_tpa() == IP
                                {
                                    // reply to the ARP request
                                    let tha = arp.get_sha();
                                    let tpa = arp.get_spa();

                                    arp.set_oper(arp::Operation::Reply);
                                    arp.set_sha(MAC);
                                    arp.set_spa(IP);
                                    arp.set_tha(tha);
                                    arp.set_tpa(tpa);
                                    iprintln!(_stim, "\n** {:?}", arp);
                                    let arp_len = arp.len();

                                    // update the Ethernet header
                                    eth.set_destination(tha);
                                    eth.set_source(MAC);
                                    eth.truncate(arp_len);
                                    iprintln!(_stim, "* {:?}", eth);

                                    iprintln!(_stim, "Tx({})", eth.as_bytes().len());
                                    enc28j60.transmit(eth.as_bytes()).ok().unwrap();
                                }
                            }
                            Err(_arp) => {
                                iprintln!(_stim, "** {:?}", _arp);
                            }
                        }
                    } else {
                        iprintln!(_stim, "Err(B)");
                    }
                }
                ether::Type::Ipv4 => {
                    if let Ok(mut ip) = ipv4::Packet::parse(eth.payload_mut()) {
                        iprintln!(_stim, "** {:?}", ip);

                        let src_ip = ip.get_source();

                        if !src_mac.is_broadcast() {
                            cache.insert(src_ip, src_mac).ok();
                        }

                        match ip.get_protocol() {
                            ipv4::Protocol::Icmp => {
                                if let Ok(mut icmp) = icmp::Packet::parse(ip.payload_mut()) {
                                    match icmp.downcast::<icmp::EchoRequest>() {
                                        Ok(request) => {
                                            iprintln!(_stim, "*** {:?}", request);

                                            let src_mac = cache
                                                .get(&src_ip)
                                                .unwrap_or_else(|| unimplemented!());
                                            let _reply: icmp::Packet<_, icmp::EchoReply, _> =
                                                request.into();
                                            iprintln!(_stim, "\n*** {:?}", _reply);

                                            // update the IP header
                                            let mut ip = ip.set_source(IP);
                                            ip.set_destination(src_ip);
                                            let _ip = ip.update_checksum();
                                            iprintln!(_stim, "** {:?}", _ip);

                                            // update the Ethernet header
                                            eth.set_destination(*src_mac);
                                            eth.set_source(MAC);
                                            iprintln!(_stim, "* {:?}", eth);

                                            led.toggle();
                                            iprintln!(_stim, "Tx({})", eth.as_bytes().len());
                                            enc28j60.transmit(eth.as_bytes()).ok().unwrap();
                                        }
                                        Err(_icmp) => {
                                            iprintln!(_stim, "*** {:?}", _icmp);
                                        }
                                    }
                                } else {
                                    iprintln!(_stim, "Err(C)");
                                }
                            }
                            ipv4::Protocol::Udp => {
                                if let Ok(mut udp) = udp::Packet::parse(ip.payload_mut()) {
                                    iprintln!(_stim, "*** {:?}", udp);

                                    if let Some(src_mac) = cache.get(&src_ip) {
                                        let src_port = udp.get_source();
                                        let dst_port = udp.get_destination();

                                        // update the UDP header
                                        udp.set_source(dst_port);
                                        udp.set_destination(src_port);
                                        udp.zero_checksum();
                                        iprintln!(_stim, "\n*** {:?}", udp);

                                        // update the IP header
                                        let mut ip = ip.set_source(IP);
                                        ip.set_destination(src_ip);
                                        let ip = ip.update_checksum();
                                        let ip_len = ip.len();
                                        iprintln!(_stim, "** {:?}", ip);

                                        // update the Ethernet header
                                        eth.set_destination(*src_mac);
                                        eth.set_source(MAC);
                                        eth.truncate(ip_len);
                                        iprintln!(_stim, "* {:?}", eth);

                                        led.toggle();
                                        iprintln!(_stim, "Tx({})", eth.as_bytes().len());
                                        enc28j60.transmit(eth.as_bytes()).ok().unwrap();
                                    }
                                }
                            }
                            _ => {}
                        }
                    } else {
                        iprintln!(_stim, "Err(D)");
                    }
                }
                _ => {}
            }
        } else {
            iprintln!(_stim, "Err(E)");
        }
    }

    // clear the pending interrupt flag
    r.EXTI.pr.write(|w| w.pr0().set_bit());
}

fn sys_tick(_t: &mut Threshold, mut r: SYS_TICK::Resources) {
    let _stim = &mut r.ITM.stim[1];

    iprint!(_stim, "{}\n", *r.SLEEP);

    *r.SLEEP = 0;
}
