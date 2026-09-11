fn meta() {
    #{
        id: "discovergy_meter",
        name: "Discovergy Smart Electricity Meter",
        category: "energy",
        category_title: "Solar & Energy Systems",
        category_icon: "☀️",
        capabilities: ["energy-meter", "smart-meter", "grid-monitoring"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    let vendor = obs.vendor.to_lower();

    name.contains("discovergy")
        || host.contains("discovergy")
        || vendor.contains("discovergy")
        || host.contains("edgy")
        || name.contains("edgy")
}
