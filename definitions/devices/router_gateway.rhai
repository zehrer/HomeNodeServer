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
    name == "fritz.box" || name.starts_with("fritz.box") || name.contains("router") || name.contains("gateway") || obs.ip.ends_with(".1")
}
