fn meta() {
    #{
        id: "govee_light",
        name: "Govee Smart LED Light",
        category: "lighting",
        category_title: "Smart Lighting",
        category_icon: "💡",
        capabilities: ["wifi", "ble", "rgb-light", "local-api"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    let vendor = obs.vendor.to_lower();

    name.contains("govee")
        || host.contains("govee")
        || vendor.contains("govee")
        || host.starts_with("led-govee")
        || host.contains("h600d")
        || name.contains("h600d")
}
