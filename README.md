# LED lamp based on ESP32c3, WS2812B LED strip and Rust

This project is a LED light that can be controlled via WiFi. It is based on the ESP32c3 microcontroller and a WS2812B 
LED strip. The LED strip can be controlled via a web interface.

Rust is used together with embassy. embassy is a project that provides async/await for embedded systems. I like 
embassy a lot, it provides a great experience von embedded systems.

## Build and flash

To build and flash the software I use the following command:

```bash
SSID=<YOUR_WIFI_SSID> PASSWORD=<YOUR_WIFI_SECRET> cargo espflash flash --bin led-lamp2 --release --monitor
```

## Issues

In rainbow mode the LED strip flickers. This is an issue in combination with the WiFi. Maybe it would help to control
the LEDs via SPI, but I did not check that yet. So for now I have no solution for this issue. There is a post in the
embassy forum about this issue: https://www.esp32.com/viewtopic.php?t=3980 (they are talking about C, but it's the same
issue).