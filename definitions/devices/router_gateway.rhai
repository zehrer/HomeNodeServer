fn meta() {
    #{
        id: "router_gateway",
        name: "Network Router / Gateway",
        category: "router",
        category_title: "Routers & Gateways",
        category_icon: "🌐",
        capabilities: ["gateway", "dns", "dhcp"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();

    if name.contains("hue") || host.contains("hue") {
        return false;
    }

    name == "fritz.box"
        || name.starts_with("fritz.box")
        || host == "fritz.box"
        || host.starts_with("fritz.box")
        || name.contains("router")
        || host.contains("router")
        || name.contains("default-gateway")
        || host.contains("default-gateway")
        || obs.ip.ends_with(".1")
}
