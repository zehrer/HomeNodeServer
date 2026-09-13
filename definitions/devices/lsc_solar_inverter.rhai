fn meta() {
    #{
        id: "lsc_solar_inverter",
        name: "LSC Micro Solar Inverter",
        category: "energy",
        category_title: "Solar & Energy Systems",
        category_icon: "☀️",
        capabilities: ["solar", "inverter", "energy-meter", "tuya-local", "wifi"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    let mac = obs.mac.to_lower();

    // Match LSC Micro Solar Converter:
    // Hostname typically "lwip0" or "lwip0.fritz.box", or specific inverter MAC
    host.starts_with("lwip0")
        || name.starts_with("lwip0")
        || mac == "4c:a9:19:39:b9:3e"
        || (host.contains("lwip") && (name.contains("solar") || host.contains("solar") || name.contains("inverter") || host.contains("inverter")))
        || name.contains("lsc solar")
        || host.contains("lsc solar")
}
