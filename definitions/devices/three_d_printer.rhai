fn meta() {
    #{
        id: "three_d_printer",
        name: "3D Printer",
        category: "3d-printer",
        category_title: "3D Printers & Makers",
        category_icon: "🧊",
        capabilities: ["3d-printing", "octoprint", "klipper", "moonraker", "http"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();

    host.contains("centauri")
        || name.contains("centauri")
        || host.contains("carbon")
        || name.contains("elegoo")
        || host.contains("elegoo")
        || name.contains("bambu")
        || host.contains("bambulab")
        || name.contains("prusa")
        || host.contains("prusa")
        || name.contains("creality")
        || host.contains("ender")
        || name.contains("voron")
        || host.contains("klipper")
        || host.contains("octoprint")
        || name.contains("3d-printer")
        || name.contains("3d drucker")
        || host.contains("3d-drucker")
}
