fn meta() {
    #{
        id: "wiz_light",
        name: "WiZ Connected Smart Light",
        category: "lighting",
        category_title: "Smart Lighting",
        category_icon: "💡",
        capabilities: ["wifi", "udp-control", "rgb-light"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();

    name.contains("wiz")
        || host.contains("wiz")
        || host.starts_with("led-wiz")
}
