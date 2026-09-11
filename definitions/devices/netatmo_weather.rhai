fn meta() {
    #{
        id: "netatmo_weather",
        name: "Netatmo Weather Station",
        category: "sensor",
        category_title: "Sensors & Detectors",
        category_icon: "🌡️",
        capabilities: ["wifi", "temperature", "humidity", "co2", "barometer", "weather"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    let vendor = obs.vendor.to_lower();

    name.contains("netatmo") || host.contains("netatmo") || vendor.contains("netatmo")
}
