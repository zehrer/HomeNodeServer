fn meta() {
    #{
        id: "miele_appliance",
        name: "Miele Home Appliance",
        category: "appliance",
        category_title: "Home Appliances",
        category_icon: "🧺",
        capabilities: ["wifi", "smart-home", "appliance", "http"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    let vendor = obs.vendor.to_lower();

    name.contains("miele")
        || host.contains("miele")
        || vendor.contains("miele")
}
