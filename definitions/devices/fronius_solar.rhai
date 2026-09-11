fn meta() {
    #{
        id: "fronius_solar",
        name: "Fronius Solar Inverter",
        category: "energy",
        category_title: "Solar & Energy Systems",
        category_icon: "☀️",
        capabilities: ["solar", "inverter", "energy-meter", "modbus", "http"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    let vendor = obs.vendor.to_lower();

    name.contains("fronius")
        || host.contains("fronius")
        || vendor.contains("fronius")
        || host.contains("symo")
        || host.contains("gen24")
}
