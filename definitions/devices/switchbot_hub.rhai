fn meta() {
    #{
        id: "switchbot_hub",
        name: "SwitchBot Hub / Device",
        category: "hub",
        category_title: "Smart Home Hubs",
        category_icon: "🎛️",
        capabilities: ["wifi", "ble", "matter", "infrared"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    let vendor = obs.vendor.to_lower();

    name.contains("switchbot")
        || host.contains("switchbot")
        || vendor.contains("woan")
        || vendor.contains("switchbot")
}
