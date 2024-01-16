#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]
// uncomment this line before  rustc 1.75.0
//#![feature(async_fn_in_trait)]
#![allow(incomplete_features)]
#[macro_use]
extern crate alloc;

use core::mem::MaybeUninit;

use embassy_executor::Spawner;
use embassy_net::{Config, ConfigV4, Ipv4Address, Ipv4Cidr, Stack, StackResources};
use embassy_net::tcp::TcpSocket;
use embassy_net_ppp::Runner;
use embassy_time::{Timer, Duration};
use embassy_rp::bind_interrupts;
use embassy_rp::peripherals::{USB, UART0};
use embassy_rp::usb::{Driver, InterruptHandler};
use embassy_rp::uart::{BufferedInterruptHandler, Uart, BufferedUart, Blocking, DataBits, StopBits, Parity};
use embassy_rp::gpio::{Level, Output};

use embedded_alloc::Heap;
use embedded_io_async::Write;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};
use heapless::Vec;

use anyhow::anyhow;

mod modem;
use crate::modem::*;

//macro_rules! singleton {
//    ($val:expr) => {{
//        type T = impl Sized;
//        static STATIC_CELL: StaticCell<T> = StaticCell::new();
//        STATIC_CELL.init_with(move || $val)
//    }};
//}

bind_interrupts!(struct Irqs {
    UART0_IRQ => BufferedInterruptHandler<UART0>;
    USBCTRL_IRQ => InterruptHandler<USB>;
});

//
// static variables
//
const HEAP_SIZE : usize = 1024 * 16;     // 16KiB 
static mut HEAP_MEM : [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
#[global_allocator]
static HEAP : Heap = Heap::empty();

//
// Tasks
//
#[embassy_executor::task]
async fn net_task(stack: &'static Stack<embassy_net_ppp::Device<'static>>) -> ! {
    stack.run().await
}

#[embassy_executor::task]
async fn logger_task(driver: Driver<'static, USB>) {
    embassy_usb_logger::run!(1024, log::LevelFilter::Debug, driver);
}

#[embassy_executor::task]
async fn ppp_task(
    stack: &'static Stack<embassy_net_ppp::Device<'static>>,
    mut runner: Runner<'static>,
    port: BufferedUart<'static, UART0>,
) -> ! {

    let config = embassy_net_ppp::Config {
        username: b"povo2.0",
        password: b"",
    };

    runner
        .run(port, config, |ipv4| {
            let Some(addr) = ipv4.address else {
                log::warn!("PPP did not provide an IP address.");
                return;
            };
            let mut dns_servers = Vec::new();
            for s in ipv4.dns_servers.iter().flatten() {
                let _ = dns_servers.push(Ipv4Address::from_bytes(&s.0));
            }
            let config = ConfigV4::Static(embassy_net::StaticConfigV4 {
                address: Ipv4Cidr::new(Ipv4Address::from_bytes(&addr.0), 0),
                gateway: None,
                dns_servers: dns_servers.clone()
            });
            stack.set_config_v4(config);
            log::info!("Got IPv4 address: addr={:?}, dns={:?}", addr, dns_servers);
        })
        .await
        .unwrap();
    unreachable!()
}

pub struct UartWrapper<'d>
{
    uart: Uart<'d, UART0, Blocking>,
}

impl <'d> UartWrapper<'d>
{
    pub fn new(t: Uart<'d, UART0, Blocking>) -> Self {
        Self{uart: t}
    }
}

impl<'d> embedded_io::ErrorType for UartWrapper<'d> {
    type Error = embassy_rp::uart::Error;
}
impl<'d> embedded_io::Read for UartWrapper<'d> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.uart.blocking_read(buf).and_then(|_| Ok(buf.len()) )
    }
}

impl<'d> embedded_io::Write for UartWrapper<'d> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.uart.blocking_write(buf).and_then(|_| Ok(buf.len()) )
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.uart.blocking_flush()
    }
}


async fn app_main(spawner: Spawner) -> anyhow::Result<()>
{
    // Initialize the allocator BEFORE use it
    {
        unsafe { HEAP.init(HEAP_MEM.as_ptr() as usize, HEAP_SIZE) }
    }

    let p = embassy_rp::init(Default::default());
    let mut led = Output::new(p.PIN_25, Level::Low);

    // Start USB logger task
    let usb_driver = Driver::new(p.USB, Irqs);
    spawner.spawn(logger_task(usb_driver)).unwrap();
    Timer::after(Duration::from_millis(1_000)).await;
    led.set_high();

    log::info!("pico-lte-ppp sample.");

    // Open serial port
    log::info!("Start setting UART.");
    let (tx_pin, rx_pin, uart0) = (p.PIN_0, p.PIN_1, p.UART0);

    static TX_BUF: StaticCell<[u8; 32]> = StaticCell::new();
    let tx_buf = &mut TX_BUF.init([0; 32])[..];
    static RX_BUF: StaticCell<[u8; 32]> = StaticCell::new();
    let rx_buf = &mut RX_BUF.init([0; 32])[..];
    let mut uart_config = embassy_rp::uart::Config::default();
    uart_config.baudrate = 115200;
    uart_config.data_bits = DataBits::DataBits8;
    uart_config.parity = Parity::ParityNone;
    uart_config.stop_bits = StopBits::STOP1;

    //let uart = Uart::new_blocking(uart0, tx_pin, rx_pin, uart_config);
    //let mut uart = UartWrapper::new(uart);

    let mut uart = BufferedUart::new(uart0, Irqs, tx_pin, rx_pin, tx_buf, rx_buf, uart_config);
    log::info!("Setting UART done.");

    log::info!("Wait LTE module wakeup.");
    loop {
        if let Ok(response) = send_cmd(&mut uart, "AT\r", 1000).await {
            if response.contains("OK") {
                log::info!("LTE wakeup.");
            }
            else {
                log::info!("Unknown response: {}, wakeup wait...", response);
            }
            break;
        }
        else {
            log::info!("LTE wakeup wait...");
        }
    }
    log::info!("Start initializing LTE.");
    lte_initialize(&mut uart).await?;
    log::info!("LTE initialize done.");
    // Generate random seed
    let seed = 0x0123_4567_89ab_cdef; // chosen by fair dice roll. guarenteed to be random.

    // Init network device
    log::info!("Start initializing network device PPP.");
    static STATE: StaticCell<embassy_net_ppp::State<4, 4>> = StaticCell::new();
    let state = STATE.init(embassy_net_ppp::State::<4, 4>::new());
    let (device, runner) = embassy_net_ppp::new(state);
    log::info!("Network device PPP initialize done.");

    // Init network stack
    log::info!("Start initializing network stack.");
    static STACK: StaticCell<Stack<embassy_net_ppp::Device<'static>>> = StaticCell::new();
    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let stack = &*STACK.init(Stack::new(
        device,
        Config::default(), // don't configure IP yet
        RESOURCES.init(StackResources::<3>::new()),
        seed,
    ));
    log::info!("Network stack initialize done.");

    // Launch network task
    log::info!("Launch network task.");
    spawner.spawn(net_task(stack)).map_err(|_| anyhow!("Spawn net_task() failed."))?;
    log::info!("Launch PPP task.");
    spawner.spawn(ppp_task(stack, runner, uart)).map_err(|_| anyhow!("Spawn ppp_task() failed."))?;

    // Then we can use it!
    log::info!("Initialize all done. Now we can use network stack!");
    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    loop {
        Timer::after(Duration::from_millis(1_000)).await;

        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        log::info!("Create socket.");
        Timer::after(Duration::from_millis(1_000)).await;

        socket.set_timeout(Some(embassy_time::Duration::from_secs(10)));
        log::info!("Set timeout.");
        Timer::after(Duration::from_millis(1_000)).await;

        let remote_endpoint = (Ipv4Address::new(142, 250, 185, 115), 80);
        log::info!("connecting...");
        Timer::after(Duration::from_millis(1_000)).await;
        let r = socket.connect(remote_endpoint).await;
        if let Err(e) = r {
            log::info!("connect error: {:?}", e);
            continue;
        }
        log::info!("connected!");
        let mut buf = [0; 1024];
        loop {
            let r = socket
                .write_all(b"GET / HTTP/1.0\r\nHost: www.mobile-j.de\r\n\r\n")
                .await;
            if let Err(e) = r {
                log::info!("write error: {:?}", e);
                break;
            }
            let n = match socket.read(&mut buf).await {
                Ok(0) => {
                    log::info!("read EOF");
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    log::info!("read error: {:?}", e);
                    break;
                }
            };
            log::info!("{}", core::str::from_utf8(&buf[..n]).unwrap());
        }
        Timer::after(Duration::from_millis(3000)).await;
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) -> !
{
    let err = app_main(spawner)
    .await;
    
    if let Err(e) = err {
        log::error!("Unrecoverable error happened. reason=\"{}\", Abort.", e);
        Timer::after(Duration::from_millis(1_000)).await;
    }

    loop {}
}

