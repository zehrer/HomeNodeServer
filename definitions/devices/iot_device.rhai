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
    let vendor = obs.vendor.to_lower();
    name.contains("shelly") || name.contains("netatmo") || name.contains("ecoflow") ||
    name.contains("sonoff") || name.contains("esp32") || name.contains("esp8266") ||
    vendor.contains("espressif") || vendor.contains("raspberry")
}
