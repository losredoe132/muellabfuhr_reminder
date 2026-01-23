#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

use embassy_net::{IpEndpoint, Ipv4Address};

use alloc::string::String;
use alloc::vec::Vec;
use defmt::info;
use embassy_executor::Spawner;
use embassy_net::udp::{UdpMetadata, UdpSocket};
use embassy_net::{
    DhcpConfig, Runner, Stack, StackResources,
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
};
use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::peripherals::{self, Peripherals};
use esp_println::{self as _, println};

use esp_hal::delay::Delay;
use esp_hal::{rmt::Rmt, time::Rate};
use esp_hal_smartled::SmartLedsAdapter;
use smart_leds::RGB8;
use smart_leds::{SmartLedsWrite as _, brightness, colors};
use time::{Date, Month, UtcDateTime};

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

const NTP_SERVER: Ipv4Address = Ipv4Address::new(129, 6, 15, 28); // time.nist.gov
const NTP_PORT: u16 = 123;
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

static RX_BUFFER_SIZE: usize = 32000;
extern crate alloc;

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

// If you are okay with using a nightly compiler, you can use the macro provided by the static_cell crate: https://docs.rs/static_cell/latest/static_cell/macro.make_static.html
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");
const N_LEDS: usize = 1;

#[derive(defmt::Format, Copy, Clone, Debug)]
#[repr(u8)]
enum Event {
    Verpackungs,
    Bio,
    Papier,
    Restmüll,
    Laubsack,
    Weihnachtsbäume,
}
#[derive(Debug)]
struct IcsEvent {
    dtstart: Option<Date>,
    event_type: Option<Event>,
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // generator version: 1.0.0

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[unsafe(link_section = ".dram2_uninit")] size: 98767);

    let mut delay = Delay::new();

    let mut led_buffer = esp_hal_smartled::smart_led_buffer!(N_LEDS);
    let mut led = {
        let frequency = Rate::from_mhz(80);
        let rmt = Rmt::new(peripherals.RMT, frequency).expect("Failed to initialize RMT0");
        SmartLedsAdapter::new(rmt.channel0, peripherals.GPIO13, &mut led_buffer)
    };
    info!("LED abstraction layer is initialized sucessfully.");

    let level = 20;

    let mut i: u8 = 0;
    loop {
        if i >= 255 {
            i = 0;
        }
        i += 1;

        let colors = RGB8 {
            r: i,
            g: i,
            b: 255 - i,
        };
        if let Err(e) = led.write(brightness([colors].into_iter(), level)) {
            info!("LED write failed: ");
        }
        delay.delay_millis(100);
    }
}

async fn wait_for_connection(stack: Stack<'_>) {
    println!("Waiting for link to be up");
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    println!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }
}
