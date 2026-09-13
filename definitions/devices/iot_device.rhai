fn meta() {
    #{
        id: "iot_device",
        name: "Smart Home & IoT",
        category: "iot",
        category_title: "Smart Home & IoT",
        category_icon: "💡",
        capabilities: ["sensor", "actuator"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    let vendor = obs.vendor.to_lower();
    // Do not match lwip0 solar converter
    if host.starts_with("lwip0") || name.starts_with("lwip0") || obs.mac.to_lower() == "4c:a9:19:39:b9:3e" {
        return false;
    }

    name.contains("shelly") || host.contains("shelly") ||
    name.contains("netatmo") || host.contains("netatmo") ||
    name.contains("ecoflow") || host.contains("ecoflow") ||
    name.contains("sonoff") || host.contains("sonoff") ||
    name.contains("esp32") || host.contains("esp32") ||
    name.contains("esp8266") || host.contains("esp8266") ||
    name.contains("lwip") || host.contains("lwip") ||
    vendor.contains("espressif") || vendor.contains("raspberry")
}
