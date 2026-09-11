fn meta() {
    #{
        id: "tasmota_plug",
        name: "Tasmota Smart Plug / Socket",
        category: "smart-plug",
        category_title: "Smart Plugs & Sockets",
        category_icon: "🔌",
        capabilities: ["wifi", "http", "mqtt", "power-monitoring"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    let vendor = obs.vendor.to_lower();

    name.contains("tasmota")
        || host.contains("tasmota")
        || host.contains("outlet")
        || host.contains("plug")
        || (vendor.contains("espressif") && (host.contains("socket") || name.contains("plug")))
}
