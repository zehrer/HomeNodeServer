fn meta() {
    #{
        id: "awtrix_clock",
        name: "Awtrix Pixel Clock",
        category: "display",
        category_title: "Smart Clocks & Displays",
        category_icon: "⏰",
        capabilities: ["wifi", "display", "http", "mqtt"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();

    name.contains("awtrix") || host.contains("awtrix")
}
