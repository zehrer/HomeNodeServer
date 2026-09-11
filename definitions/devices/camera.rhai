fn meta() {
    #{
        id: "camera",
        name: "Security Camera",
        category: "camera",
        category_title: "Cameras",
        category_icon: "📷",
        capabilities: ["video", "motion"]
    }
}

fn identify(obs) {
    let name = obs.name.to_lower();
    name.contains("camera") || name.contains("blink") || name.contains("ring") || name.contains("cam")
}
