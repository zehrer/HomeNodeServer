fn meta() {
    #{
        id: "philips_hue",
        name: "Philips Hue Bridge / Gateway",
        category: "hub",
        category_title: "Smart Home Hubs",
        category_icon: "🎛️",
        capabilities: ["zigbee", "hub", "matter", "bridge", "http"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    let vendor = obs.vendor.to_lower();

    name.contains("hue")
        || host.contains("hue")
        || vendor.contains("philips")
        || host.contains("hue-bridge")
        || host.contains("hue-gateway")
}
