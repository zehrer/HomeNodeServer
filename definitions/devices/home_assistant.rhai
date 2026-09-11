fn meta() {
    #{
        id: "home_assistant",
        name: "Home Assistant Smart Hub",
        category: "hub",
        category_title: "Smart Home Hubs",
        category_icon: "🎛️",
        capabilities: ["web_ui", "matter_controller", "automation_engine", "smart_home_hub"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    let vendor = obs.vendor.to_lower();

    name.contains("home assistant")
        || name.contains("homeassistant")
        || name.contains("hassio")
        || host.contains("homeassistant")
        || host.contains("hassio")
        || vendor.contains("home assistant")
        || vendor.contains("nabu casa")
}
