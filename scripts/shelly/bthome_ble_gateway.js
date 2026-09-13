/**
 * =========================================================================
 * HomeNode - Shelly BLE Gateway Script
 * =========================================================================
 * Designed for Shelly Gen2, Gen3, and Pro series (Plus 1PM, Pro 3EM, etc.)
 * 
 * Functions:
 * - Continuously listens for passive Bluetooth Low Energy (BLE) advertisements.
 * - Filters for BTHome V2 broadcasts (Shelly BLU Button 1, BLU Door/Window,
 *   BLU Motion, BLU H&T, and custom BTHome sensors).
 * - Forwards sensor payloads to your local HomeNode Server over Wi-Fi / Ethernet.
 * - Supports multi-gateway mesh: run this script across multiple Shellys to cover
 *   every room and floor with whole-home coverage and server-side deduplication.
 * 
 * Setup instructions:
 * 1. Open your Shelly Web Interface in your browser (e.g. http://192.168.178.xx).
 * 2. In "Settings" -> "Bluetooth": Enable "Bluetooth" and "Bluetooth Gateway".
 * 3. In the left menu, click "Scripts" -> "Add Script".
 * 4. Name the script: "HomeNode BLE Gateway".
 * 5. Paste this entire code into the script editor.
 * 6. Update CONFIG.homenode_url with the IP address of your HomeNode Server.
 * 7. Click "Save", click "Start", and toggle "Run on startup" (Auto start) to ON.
 * =========================================================================
 */

let CONFIG = {
  // Replace with the LAN IP address of your HomeNode Server (port 8124)
  homenode_url: "http://192.168.178.100:8124/bthome",

  // Ignore weak signals below this threshold (in dBm)
  min_rssi: -90,

  // Set to true to print forward events in Shelly console
  debug: false,
};

let BTHOME_SVC_ID_STR = "fcd2";
let SHELLY_DEVICE_ID = (Shelly.getDeviceInfo && Shelly.getDeviceInfo().id) ? Shelly.getDeviceInfo().id : "shelly-gateway";

console.log("[HomeNode-BLE] Starting BTHome Gateway on", SHELLY_DEVICE_ID, "target:", CONFIG.homenode_url);

function onScanResult(ev, res) {
  if (ev !== BLE.Scanner.SCAN_RESULT) return;

  // Filter: Must contain BTHome V2 service data (0xFCD2)
  if (!res.service_data || typeof res.service_data[BTHOME_SVC_ID_STR] === "undefined") {
    return;
  }

  // Filter: Minimum RSSI
  if (typeof res.rssi === "number" && res.rssi < CONFIG.min_rssi) {
    return;
  }

  let payloadHex = res.service_data[BTHOME_SVC_ID_STR];

  if (CONFIG.debug) {
    console.log("[HomeNode-BLE] Fwd sensor:", res.addr, "RSSI:", res.rssi, "Data:", payloadHex);
  }

  // Forward packet to HomeNode Server HTTP webhook
  Shelly.call(
    "HTTP.Request",
    {
      method: "POST",
      url: CONFIG.homenode_url,
      timeout: 3,
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        gateway: SHELLY_DEVICE_ID,
        mac: res.addr,
        rssi: res.rssi,
        data: payloadHex
      })
    },
    function (result, error_code, error_msg) {
      if (error_code !== 0 && CONFIG.debug) {
        console.log("[HomeNode-BLE] HTTP error:", error_code, error_msg);
      }
    }
  );
}

// Start infinite passive BLE scan
BLE.Scanner.Start(
  {
    duration_ms: BLE.Scanner.INFINITE_SCAN,
    active: false // Passive scan consumes zero additional sensor battery
  },
  onScanResult
);
