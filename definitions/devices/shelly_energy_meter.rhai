fn meta() {
    #{
        id: "shelly_energy_meter",
        name: "Shelly Pro 3EM / Energy Meter",
        category: "energy",
        category_title: "Solar & Energy Systems",
        category_icon: "☀️",
        capabilities: ["wifi", "ethernet", "energy-meter", "power-monitoring", "3-phase", "http", "rpc"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();

    (host.contains("shelly") || name.contains("shelly"))
        && (host.contains("3em") || host.contains("pro3em") || host.contains("em3") || name.contains("3em"))
}
