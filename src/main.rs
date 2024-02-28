#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]
#![feature(const_mut_refs)]

extern crate alloc;

use alloc::borrow::ToOwned;
use core::mem::MaybeUninit;
use core::sync::atomic::AtomicBool;
use embassy_executor::Executor;
use embassy_net::{Config, Stack, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer};
use embedded_svc::wifi::{ClientConfiguration, Configuration, Wifi};
use esp_backtrace as _;
use esp_hal_common::rmt::Channel;
use esp_hal_smartled::SmartLedsAdapter;
use hal::{
    clock::ClockControl,
    embassy,
    gpio::{Output, PushPull},
    peripherals::Peripherals,
    prelude::*,
    timer::TimerGroup,
    Rmt, IO,
};

use esp_wifi::wifi::WifiStaDevice;
use esp_wifi::{
    initialize,
    wifi::{WifiController, WifiDevice, WifiEvent, WifiState},
    EspWifiInitFor,
};

use hal::gpio::Gpio8;
use hal::{systimer::SystemTimer, Rng};
use log::{error, info};
use palette::{FromColor, Hsv, Srgb};
use picoserve::{response::IntoResponse, routing::get, KeepAlive, Router, Timeouts};
use picoserve::extract::{Form, FromRequest};
use smart_leds::{RGB, SmartLedsWrite};
use smart_leds::RGB8;
use static_cell::make_static;

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");

static NET_STATUS: AtomicBool = AtomicBool::new(false);

static LED_STRIPE_MODE: Mutex<CriticalSectionRawMutex, RGB8> = Mutex::new(RGB8::new(0, 0, 0));

const NUM_LEDS: usize = 18;

struct EmbassyTimer;

impl picoserve::Timer for EmbassyTimer {
    type Duration = embassy_time::Duration;
    type TimeoutError = embassy_time::TimeoutError;

    async fn run_with_timeout<F: core::future::Future>(
        &mut self,
        duration: Self::Duration,
        future: F,
    ) -> Result<F::Output, Self::TimeoutError> {
        embassy_time::with_timeout(duration, future).await
    }
}

#[global_allocator]
static ALLOCATOR: esp_alloc::EspHeap = esp_alloc::EspHeap::empty();

fn init_heap() {
    const HEAP_SIZE: usize = 32 * 1024;
    static mut HEAP: MaybeUninit<[u8; HEAP_SIZE]> = MaybeUninit::uninit();

    unsafe {
        ALLOCATOR.init(HEAP.as_mut_ptr() as *mut u8, HEAP_SIZE);
    }
}

#[embassy_executor::task]
async fn blink_blue(mut pin: Gpio8<Output<PushPull>>) {
    loop {
        if NET_STATUS.load(core::sync::atomic::Ordering::Relaxed) {
            pin.toggle().unwrap();
        } else {
            pin.set_high().unwrap();
        }
        Timer::after(Duration::from_millis(800)).await;
    }
}

#[embassy_executor::task]
async fn led_stripe(mut led: SmartLedsAdapter<Channel<0>, 0, 433>) {
    let mut h: f32 = 0.0;
    let s: f32 = 1.0;
    let v: f32 = 0.8;
    let mut leds: [RGB8; NUM_LEDS] = [RGB8::default(); NUM_LEDS];
    let mut led_mode: RGB8;
    let led_init = RGB::new(0u8,0u8,0u8);
    loop {
        h += 2.0;
        if h > 360.0 {
            h = 0.0;
        }

        {
            led_mode = LED_STRIPE_MODE.lock().await.to_owned().clone();
        }

        if led_mode == led_init {
            for i in 0..NUM_LEDS {
                let rgb = Srgb::from_color(Hsv::new(h + (i as f32 * 20.0), s, v));
                let (r, g, b) = (
                    (rgb.red * 255.0) as u8,
                    (rgb.green * 255.0) as u8,
                    (rgb.blue * 255.0) as u8,
                );
                leds[i] = RGB8::new(r, g, b);
            }
        } else {
            info!("Static mode: {:?}", led_mode);
            for i in 0..NUM_LEDS {
                leds[i] = led_mode.clone();
            }
        }
        led.write(leds.iter().cloned()).unwrap();
        Timer::after(Duration::from_millis(25)).await;
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    info!("start connection task");
    info!("Device capabilities: {:?}", controller.get_capabilities());
    loop {
        match esp_wifi::wifi::get_wifi_state() {
            WifiState::StaConnected => {
                // wait until we're no longer connected
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                Timer::after(Duration::from_millis(5000)).await
            }
            _ => {}
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: SSID.try_into().unwrap(),
                password: PASSWORD.try_into().unwrap(),
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            info!("Starting wifi");
            controller.start().await.unwrap();
            info!("Wifi started!");
        }
        info!("About to connect...");

        match controller.connect().await {
            Ok(_) => info!("Wifi connected!"),
            Err(e) => {
                error!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>) {
    stack.run().await
}

async fn get_root() -> impl IntoResponse {
    picoserve::response::File::html(include_str!("index.html"))
}

enum BadRequest {
    ReadError,
}

impl IntoResponse for BadRequest {
    async fn write_to<
        R: picoserve::io::Read,
        W: picoserve::response::ResponseWriter<Error = R::Error>,
    >(
        self,
        connection: picoserve::response::Connection<'_, R>,
        response_writer: W,
    ) -> Result<picoserve::ResponseSent, W::Error> {

        (
            picoserve::response::status::BAD_REQUEST,
            format_args!("Read Error"),
        )
            .write_to(connection, response_writer)
            .await
    }

}

#[derive(serde::Deserialize)]
struct FormValue {
    mode: heapless::String<32>,
    r: u8,
    g: u8,
    b: u8
}

struct LedSettings {
    mode: LedMode,
    r: u8,
    g: u8,
    b: u8
}

#[derive(Debug)]
enum LedMode {
    Rainbow,
    Static
}

impl<State> FromRequest<State> for LedSettings {
    type Rejection = BadRequest;

    async fn from_request<R: picoserve::io::Read>(
        _state: &State,
        _request_parts: picoserve::request::RequestParts<'_>,
        request_body: picoserve::request::RequestBody<'_, R>,
    ) -> Result<Self, Self::Rejection> {
        let form: Form<FormValue> = Form::from_request(_state, _request_parts, request_body).await.map_err(|_err| BadRequest::ReadError)?;
        Ok(LedSettings {
            mode: match form.mode.as_str() {
                "rainbow" => LedMode::Rainbow,
                "static" => LedMode::Static,
                _ => LedMode::Rainbow
            },
            r: form.r,
            g: form.g,
            b: form.b
        })
    }
}

async fn post_root(LedSettings { mode, r, g, b }: LedSettings) -> impl IntoResponse {
    info!("mode: {:?}, r: {r}, g: {g}, b: {b}", mode);
    match mode {
        LedMode::Rainbow => {
            let mut rgb = LED_STRIPE_MODE.lock().await;
            rgb.r = 0;
            rgb.g = 0;
            rgb.b = 0;
        }
        LedMode::Static => {
            let mut rgb = LED_STRIPE_MODE.lock().await;
            rgb.r = r;
            rgb.g = g;
            rgb.b = b;
        }
    }
    picoserve::response::Redirect::to("/")
}

#[embassy_executor::task]
async fn web_task(
    stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>,
    config: &'static picoserve::Config<Duration>,
) -> ! {
    let mut rx_buffer = [0; 1024];
    let mut tx_buffer = [0; 1024];

    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    info!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            info!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    loop {
        let mut socket = embassy_net::tcp::TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);

        info!("Listening on TCP:80...");
        NET_STATUS.store(true, core::sync::atomic::Ordering::Relaxed);
        if let Err(e) = socket.accept(80).await {
            log::warn!("accept error: {:?}", e);
            continue;
        }

        info!("Received connection from {:?}", socket.remote_endpoint());

        let app = Router::new()
            .route("/", get(get_root).post(post_root));

        info!("Serving...");
        match picoserve::serve(&app, config, &mut [0; 2048], socket).await {
            Ok(handled_requests_count) => {
                info!("{handled_requests_count} requests handled");
            }
            Err(err) => error!("{err:?}"),
        }
        NET_STATUS.store(false, core::sync::atomic::Ordering::Relaxed);
    }
}

#[entry]
fn main() -> ! {
    init_heap();
    let peripherals = Peripherals::take();
    let system = peripherals.SYSTEM.split();
    let clocks = ClockControl::max(system.clock_control).freeze();
    // let mut delay = Delay::new(&clocks);

    // setup logger
    // To change the log_level change the env section in .cargo/config.toml
    // or remove it and set ESP_LOGLEVEL manually before running cargo run
    // this requires a clean rebuild because of https://github.com/rust-lang/cargo/issues/10358
    esp_println::logger::init_logger_from_env();
    info!("Logger is setup");

    let io = IO::new(peripherals.GPIO, peripherals.IO_MUX);
    let mut pin8 = io.pins.gpio8.into_push_pull_output();
    pin8.set_high().unwrap();

    let executor = make_static!(Executor::new());

    let timer_group = TimerGroup::new(peripherals.TIMG0, &clocks);

    let timer = SystemTimer::new(peripherals.SYSTIMER).alarm0;
    let init = initialize(
        EspWifiInitFor::Wifi,
        timer,
        Rng::new(peripherals.RNG),
        system.radio_clock_control,
        &clocks,
    )
    .unwrap();

    let wifi = peripherals.WIFI;
    let (wifi_interface, controller) =
        esp_wifi::wifi::new_with_mode(&init, wifi, WifiStaDevice).unwrap();

    let config = Config::dhcpv4(Default::default());
    let seed = 1234; // very random, very secure seed

    // Init network stack
    let stack = &*make_static!(Stack::new(
        wifi_interface,
        config,
        make_static!(StackResources::<3>::new()),
        seed
    ));

    let config = make_static!(picoserve::Config {
        timeouts: Timeouts {
            start_read_request: Some(Duration::from_secs(5)),
            read_request: Some(Duration::from_secs(1)),
            write: Some(Duration::from_secs(1)),
        },
        connection: KeepAlive::Close,
    });

    let rmt = Rmt::new(peripherals.RMT, 80u32.MHz(), &clocks).unwrap();
    let rmt_buffer = [0u32; NUM_LEDS * 24 + 1];
    let led = SmartLedsAdapter::new(rmt.channel0, io.pins.gpio0, rmt_buffer);

    embassy::init(&clocks, timer_group);

    executor.run(|spawner| {
        spawner.spawn(blink_blue(pin8)).unwrap();
        spawner.spawn(connection(controller)).unwrap();
        spawner.spawn(led_stripe(led)).unwrap();
        spawner.spawn(net_task(stack)).unwrap();
        spawner.spawn(web_task(stack, config)).unwrap();
    })
}
