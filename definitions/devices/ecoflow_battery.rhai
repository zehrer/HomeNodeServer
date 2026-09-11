fn meta() {
    #{
        id: "ecoflow_battery",
        name: "EcoFlow Portable Power & Battery",
        category: "energy",
        category_title: "Solar & Energy Systems",
        category_icon: "☀️",
        capabilities: ["battery", "solar", "energy-storage", "wifi", "ble"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    let vendor = obs.vendor.to_lower();

    name.contains("ecoflow") || host.contains("ecoflow") || vendor.contains("ecoflow")
}
