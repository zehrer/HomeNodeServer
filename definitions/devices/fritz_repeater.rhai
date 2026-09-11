fn meta() {
    #{
        id: "fritz_repeater",
        name: "AVM FRITZ!Repeater",
        category: "router",
        category_title: "Routers & Gateways",
        category_icon: "🌐",
        capabilities: ["wifi", "mesh", "repeater", "http"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    let vendor = obs.vendor.to_lower();

    (host.contains("repeater") || name.contains("repeater"))
        && (vendor.contains("avm") || vendor.contains("fritz") || host.contains("fritz") || name.contains("fritz"))
}
