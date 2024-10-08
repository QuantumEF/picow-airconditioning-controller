//! This example uses the RP Pico W board Wifi chip (cyw43).
//! Connects to specified Wifi network and creates a TCP endpoint on port 1234.

#![no_std]
#![no_main]
#![allow(async_fn_in_trait)]
use core::sync::atomic::Ordering;
use heapless::String;

use cyw43_pio::PioSpi;
use defmt::*;
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Config as IPConfig, Stack, StackResources};
use embassy_rp::clocks::clk_sys_freq;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, PIO0, PIO1, UART0};
use embassy_rp::pio::{InterruptHandler as PIOInterruptHandler, Pio};
use embassy_rp::{
    bind_interrupts,
    uart::{self, InterruptHandler as UARTInterruptHandler},
};
use embassy_time::{Duration, Timer};
use embedded_io_async::Write;

use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

mod dht11;
mod temp_controller;
use dht11::DHT11;
use temp_controller::{TempController, SHARED_HUMID, SHARED_TEMP};
mod uart_cli;
use uart_cli::uart_cli;

bind_interrupts!(struct PIOIrqs {
    PIO0_IRQ_0 => PIOInterruptHandler<PIO0>;
    PIO1_IRQ_0 => PIOInterruptHandler<PIO1>;
});

bind_interrupts!(struct UARTIrqs {
    UART0_IRQ  => UARTInterruptHandler<UART0>;
});

static CONTROLLER: StaticCell<TempController> = StaticCell::new();

const WIFI_NETWORK: &str = include_str!("wifi_network");
const WIFI_PASSWORD: &str = include_str!("wifi_password");

#[embassy_executor::task]
async fn wifi_task(
    runner: cyw43::Runner<'static, Output<'static>, PioSpi<'static, PIO0, 0, DMA_CH0>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<cyw43::NetDriver<'static>>) -> ! {
    stack.run().await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Hello World! {}", clk_sys_freq());

    let p = embassy_rp::init(Default::default());

    let controller: &'static mut TempController = CONTROLLER.init(TempController::new(
        22,
        Duration::from_secs(10),
        Duration::from_secs(10),
    ));

    // Safety: I don't care about race conditions.
    let test1 = controller as *mut TempController;
    let test2 = controller as *mut TempController;

    let config = uart::Config::default();
    let uart = uart::Uart::new(
        p.UART0, p.PIN_0, p.PIN_1, UARTIrqs, p.DMA_CH1, p.DMA_CH2, config,
    );

    // let fw = include_bytes!("../cyw43-firmware/43439A0.bin");
    // let clm = include_bytes!("../cyw43-firmware/43439A0_clm.bin");

    // To make flashing faster for development, you may want to flash the firmwares independently
    // at hardcoded addresses, instead of baking them into the program with `include_bytes!`:
    //     probe-rs download 43439A0.bin --format bin --chip RP2040 --base-address 0x10100000
    //     probe-rs download 43439A0_clm.bin --format bin --chip RP2040 --base-address 0x10140000
    let fw = unsafe { core::slice::from_raw_parts(0x10100000 as *const u8, 230321) };
    let clm = unsafe { core::slice::from_raw_parts(0x10140000 as *const u8, 4752) };

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let pio1 = Pio::new(p.PIO1, PIOIrqs);

    let mut pio0 = Pio::new(p.PIO0, PIOIrqs);
    let spi = PioSpi::new(
        &mut pio0.common,
        pio0.sm0,
        pio0.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        p.DMA_CH0,
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let state = STATE.init(cyw43::State::new());
    let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, fw).await;
    unwrap!(spawner.spawn(wifi_task(runner)));

    control.init(clm).await;
    control
        .set_power_management(cyw43::PowerManagementMode::PowerSave)
        .await;

    let config = IPConfig::dhcpv4(Default::default());
    //let config = embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
    //    address: Ipv4Cidr::new(Ipv4Address::new(192, 168, 69, 2), 24),
    //    dns_servers: Vec::new(),
    //    gateway: Some(Ipv4Address::new(192, 168, 69, 1)),
    //});

    // Generate random seed
    let seed = 0x0123_4567_89ab_cdef; // chosen by fair dice roll. guarenteed to be random.

    // Init network stack
    static STACK: StaticCell<Stack<cyw43::NetDriver<'static>>> = StaticCell::new();
    static RESOURCES: StaticCell<StackResources<2>> = StaticCell::new();
    let stack = &*STACK.init(Stack::new(
        net_device,
        config,
        RESOURCES.init(StackResources::<2>::new()),
        seed,
    ));

    unwrap!(spawner.spawn(uart_cli(uart, stack, test1)));

    unwrap!(spawner.spawn(net_task(stack)));

    loop {
        //control.join_open(WIFI_NETWORK).await;
        match control.join_wpa2(WIFI_NETWORK, WIFI_PASSWORD).await {
            Ok(_) => break,
            Err(err) => {
                info!("join failed with status={}", err.status);
            }
        }
    }

    // Wait for DHCP, not necessary when using static IP
    info!("waiting for DHCP...");
    while !stack.is_config_up() {
        Timer::after_millis(100).await;
    }
    info!("DHCP is now up!");

    // And now we can use it!

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];
    let mut buf = [0; 4096];

    let mut output_string = String::<64>::new();
    let mut temperature_buffer = itoa::Buffer::new();
    let mut humidity_buffer = itoa::Buffer::new();

    let mut dht11_ctl = DHT11::new(pio1, p.PIN_15);

    //Note first few values are usually shit :/ need to investigate.
    Timer::after_secs(1).await;
    let (initial_temperature, initial_humidity) = dht11_ctl.get_temperature_humidity();

    SHARED_TEMP.store(initial_temperature, Ordering::Relaxed);
    SHARED_HUMID.store(initial_humidity, Ordering::Relaxed);

    unwrap!(spawner.spawn(temp_controller::temp_controller_task(
        dht11_ctl, test2, p.PIN_13,
    )));

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));

        control.gpio_set(0, false).await;
        info!("Listening on TCP:1234...");
        if let Err(e) = socket.accept(1234).await {
            warn!("accept error: {:?}", e);
            continue;
        }

        info!("Received connection from {:?}", socket.remote_endpoint());
        control.gpio_set(0, true).await;

        loop {
            let _ = match socket.read(&mut buf).await {
                Ok(0) => {
                    warn!("read EOF");
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    warn!("read error: {:?}", e);
                    break;
                }
            };

            let temperature = SHARED_TEMP.load(Ordering::Relaxed);
            let humidity = SHARED_HUMID.load(Ordering::Relaxed);
            let temperature_str = temperature_buffer.format(temperature);
            let humidity_str = humidity_buffer.format(humidity);
            output_string.clear();
            let _ = output_string.push_str(temperature_str);
            let _ = output_string.push(',');
            let _ = output_string.push_str(humidity_str);
            let _ = output_string.push('\n');

            match socket.write_all(output_string.as_bytes()).await {
                Ok(()) => {}
                Err(e) => {
                    warn!("write error: {:?}", e);
                    break;
                }
            };
        }
    }
}
