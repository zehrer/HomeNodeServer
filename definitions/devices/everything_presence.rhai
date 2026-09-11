fn meta() {
    #{
        id: "everything_presence",
        name: "Everything Presence Sensor",
        category: "sensor",
        category_title: "Sensors & Detectors",
        category_icon: "👁️",
        capabilities: ["mmwave", "pir", "illuminance", "temperature", "humidity", "http"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    let host = obs.hostname.to_lower();

    name.contains("everything-presence")
        || host.contains("everything-presence")
        || host.contains("everything_presence")
        || host.contains("ep1-")
        || host.contains("epl-")
}
