fn meta() {
    #{
        id: "printer",
        name: "Network Printer",
        category: "printer",
        category_title: "Printers",
        category_icon: "🖨️",
        capabilities: ["ipp", "printing"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    name.contains("printer") || name.contains("epson") || name.contains("brother") || name.contains("canon") || name.contains("hp-")
}
