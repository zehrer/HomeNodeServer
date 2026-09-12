fn meta() {
    #{
        id: "computer",
        name: "Computer / Laptop",
        category: "computer",
        category_title: "Computers & Laptops",
        category_icon: "💻",
        capabilities: ["lan", "ssh"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();
    let vendor = obs.vendor.to_lower();

    if name.contains("macbook") || name.contains("imac") || name.contains("macmini") {
        return true;
    }
    if name.contains("desktop") || name.contains("laptop") || name.contains("pc") || name.contains("notebook") {
        return true;
    }
    if name.contains("mbp") || name.contains("mba") || name.contains("thinkpad") || name.contains("elitebook") || name.contains("probook") || name.contains("surface") {
        return true;
    }
    if host.contains("macbook") || host.contains("imac") || host.contains("macmini") {
        return true;
    }
    if host.contains("desktop") || host.contains("laptop") || host.contains("pc") || host.contains("notebook") {
        return true;
    }
    if host.contains("mbp") || host.contains("mba") || host.contains("thinkpad") || host.contains("elitebook") || host.contains("probook") || host.contains("surface") {
        return true;
    }
    if host.contains("hensoldt") {
        return true;
    }
    if name.contains("homenode") || host.contains("homenode") {
        return true;
    }
    if name.contains("workstation") || host.contains("workstation") || name.contains("server") || host.contains("server") {
        return true;
    }
    if name.contains("windows") || host.contains("windows") || name.contains("win10") || host.contains("win10") || name.contains("win11") || host.contains("win11") {
        return true;
    }

    let is_printer = name.contains("print") || name.contains("laserjet") || name.contains("deskjet") || name.contains("officejet")
        || host.contains("print") || host.contains("laserjet") || host.contains("deskjet") || host.contains("officejet");

    if !is_printer {
        if vendor.contains("hp") || vendor.contains("hewlett-packard") || vendor.contains("dell") || vendor.contains("lenovo") || vendor.contains("acer") || vendor.contains("asus") {
            return true;
        }
    }

    false
}
