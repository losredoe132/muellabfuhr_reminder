#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

use alloc::string::String;
use alloc::vec::Vec;
use defmt::info;
use embassy_executor::Spawner;
use embassy_net::{
    DhcpConfig, Runner, Stack, StackResources,
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
};
use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use esp_println::{self as _, println};
use esp_radio::wifi::{
    ClientConfig, ModeConfig, ScanConfig, WifiController, WifiDevice, WifiEvent, WifiStaState,
};
use reqwless::client::{HttpClient, TlsConfig};
use utc_dt::date::UTCDate;
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

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

#[derive(Copy, Clone, Debug)]
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
    dtstart: UTCDate,
    event_type: Event,
}

fn parse_yyyymmdd(s: &str) -> Result<UTCDate, &'static str> {
    if s.len() != 8 {
        return Err("Expected 8 characters (YYYYMMDD)");
    }

    let year = s[0..4].parse::<u64>().map_err(|_| "Invalid year")?;
    let month = s[4..6].parse::<u8>().map_err(|_| "Invalid month")?;
    let day = s[6..8].parse::<u8>().map_err(|_| "Invalid day")?;

    UTCDate::try_from_components(year, month, day).map_err(|_| "Invalid date")
}

fn extract_ics_event(ics_document: String) -> Vec<IcsEvent> {
    let mut ics_events: Vec<IcsEvent> = Vec::new();
    let mut event_type: Option<Event> = None;
    let mut start_ts: Option<UTCDate> = None;

    for line_str in ics_document.lines() {
        let line = line_str.trim_end();

        if line.starts_with("DTSTART;") {
            assert!(line.starts_with("DTSTART;TZID=Europe/Berlin;VALUE=DATE:"),);
            assert!(line.len() == 46, "Line length: {}", line.len());
            start_ts = Some(parse_yyyymmdd(&line[38..]).unwrap());
        } else if line.starts_with("SUMMARY:") {
            let event_name = line[8..].trim();
            match event_name {
                "Abfuhr gelbe Wertstofftonne/-sack" => {
                    event_type = Some(Event::Verpackungs);
                }
                "Abfuhr grüne Biotonne" => {
                    event_type = Some(Event::Bio);
                }
                "Abfuhr blaue Papiertonne" => {
                    event_type = Some(Event::Papier);
                }
                "Abfuhr schwarze Restmülltonne" => {
                    event_type = Some(Event::Restmüll);
                }
                "Abfuhr Laubsäcke" => {
                    event_type = Some(Event::Laubsack);
                }
                "Abfuhr Weihnachtsbäume" => {
                    event_type = Some(Event::Weihnachtsbäume);
                }
                _ => {
                    println!("Unknown Event: {}", line); // Placeholder
                }
            }
        } else if line == "END:VEVENT" {
            assert!(start_ts.is_some());
            assert!(event_type.is_some());
            //println!("{:?} @ {:?}", event_type.unwrap(), start_ts.unwrap());
            ics_events.push(IcsEvent {
                dtstart: start_ts.unwrap(),
                event_type: event_type.unwrap(),
            });
        }
    }
    return ics_events;
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    // generator version: 1.0.0

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[unsafe(link_section = ".dram2_uninit")] size: 98767);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    info!("Embassy initialized!");

    // let radio_init = esp_radio::init().expect("Failed to initialize Wi-Fi/BLE controller");
    let radio_init = &*mk_static!(
        esp_radio::Controller<'static>,
        esp_radio::init().expect("Failed to initialize Wi-Fi/BLE controller")
    );

    let (wifi_controller, interfaces) =
        esp_radio::wifi::new(&radio_init, peripherals.WIFI, Default::default())
            .expect("Failed to initialize Wi-Fi controller");

    let wifi_interface = interfaces.sta;

    let rng = Rng::new();
    let net_seed = rng.random() as u64 | ((rng.random() as u64) << 32);
    let tls_seed = rng.random() as u64 | ((rng.random() as u64) << 32);

    let dhcp_config = DhcpConfig::default();
    let config = embassy_net::Config::dhcpv4(dhcp_config);

    // Init network stack
    let (stack, runner) = embassy_net::new(
        wifi_interface,
        config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        net_seed,
    );

    spawner.spawn(connection(wifi_controller)).ok();
    spawner.spawn(net_task(runner)).ok();

    wait_for_connection(stack).await;

    let s: String = access_website(stack, tls_seed).await;
    let events = extract_ics_event(s);
    info!("Extracted {} events", events.len());
    loop {}
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

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    println!("Device capabilities: {:?}", controller.capabilities());
    loop {
        match esp_radio::wifi::sta_state() {
            WifiStaState::Connected => {
                // wait until we're no longer connected
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                Timer::after(Duration::from_millis(5000)).await
            }
            _ => {}
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = ModeConfig::Client(
                ClientConfig::default()
                    .with_ssid(SSID.into())
                    .with_password(PASSWORD.into()),
            );
            controller.set_config(&client_config).unwrap();
            println!("Starting wifi");
            controller.start_async().await.unwrap();
            println!("Wifi started!");

            println!("Scan");
            let scan_config = ScanConfig::default().with_max(10);
            let result = controller
                .scan_with_config_async(scan_config)
                .await
                .unwrap();
            for ap in result {
                println!("{:?}", ap);
            }
        }
        println!("About to connect...");

        match controller.connect_async().await {
            Ok(_) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {:?}", e);
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}

async fn access_website(stack: Stack<'_>, tls_seed: u64) -> String {
    let mut rx_buffer = [0; RX_BUFFER_SIZE];
    let mut tx_buffer = [0; 4096];
    let dns = DnsSocket::new(stack);
    let tcp_state = TcpClientState::<1, 4096, RX_BUFFER_SIZE>::new();
    let tcp = TcpClient::new(stack, &tcp_state);

    let tls = TlsConfig::new(
        tls_seed,
        &mut rx_buffer,
        &mut tx_buffer,
        reqwless::client::TlsVerify::None,
    );

    let mut client = HttpClient::new_with_tls(&tcp, &dns, tls);
    let mut buffer = [0u8; RX_BUFFER_SIZE];
    let mut http_req = client
        .request(
            reqwless::request::Method::GET,
            "https://backend.stadtreinigung.hamburg/kalender/abholtermine.ics?hnIds=44353",
        )
        .await
        .unwrap();
    let response = http_req.send(&mut buffer).await.unwrap();

    info!("Got response");
    let res = response.body().read_to_end().await.unwrap();

    let content = core::str::from_utf8(res).unwrap();
    let mut s = String::new();
    s.push_str(content);
    return s;
}
