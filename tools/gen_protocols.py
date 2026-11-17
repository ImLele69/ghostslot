#!/usr/bin/env python3
# builds data/protocols.json
#
# usage: gen_protocols.py <reports_dir> <mcdata_dir>
#
# reports_dir holds one folder per version with the vanilla `--reports` output
# (out/reports/packets.json) and the server.jar it came from. vanilla only emits
# packets.json from 1.21 on, so older protocols come from PrismarineJS
# minecraft-data protocol.json files named <version>.json in mcdata_dir
import json
import sys
import zipfile
from pathlib import Path

WANTED = {
    "play_cb": ["open_screen", "container_close", "container_set_content",
                "container_set_slot", "container_set_data", "start_configuration",
                "disconnect", "level_chunk_with_light", "forget_level_chunk"],
    "play_sb": ["container_click", "container_close", "container_button_click",
                "configuration_acknowledged"],
    "config_cb": ["finish_configuration"],
    "config_sb": ["finish_configuration"],
}

PRISMARINE_NAMES = {
    "play_cb": {"open_window": "open_screen", "close_window": "container_close",
                "window_items": "container_set_content", "set_slot": "container_set_slot",
                "craft_progress_bar": "container_set_data",
                "start_configuration": "start_configuration",
                "kick_disconnect": "disconnect", "map_chunk": "level_chunk_with_light",
                "unload_chunk": "forget_level_chunk"},
    "play_sb": {"window_click": "container_click", "close_window": "container_close",
                "enchant_item": "container_button_click",
                "configuration_acknowledged": "configuration_acknowledged"},
    "config_cb": {"finish_configuration": "finish_configuration"},
    "config_sb": {"finish_configuration": "finish_configuration"},
}


def from_vanilla(packets):
    sections = {"play_cb": ("play", "clientbound"), "play_sb": ("play", "serverbound"),
                "config_cb": ("configuration", "clientbound"),
                "config_sb": ("configuration", "serverbound")}
    out = {}
    for key, (phase, direction) in sections.items():
        table = packets.get(phase, {}).get(direction, {})
        out[key] = {n: table[f"minecraft:{n}"]["protocol_id"]
                    for n in WANTED[key] if f"minecraft:{n}" in table}
    return out


def mappings(proto, phase, direction):
    try:
        packet = proto[phase][direction]["types"]["packet"]
    except KeyError:
        return {}
    for field in packet[1]:
        if field["name"] == "name":
            return {v: int(k, 16) for k, v in field["type"][1]["mappings"].items()}
    return {}


def from_prismarine(proto):
    sections = {"play_cb": ("play", "toClient"), "play_sb": ("play", "toServer"),
                "config_cb": ("configuration", "toClient"),
                "config_sb": ("configuration", "toServer")}
    out = {}
    for key, (phase, direction) in sections.items():
        table = mappings(proto, phase, direction)
        out[key] = {PRISMARINE_NAMES[key][n]: i for n, i in table.items()
                    if n in PRISMARINE_NAMES[key]}
    return out


def main():
    reports, mcdata = Path(sys.argv[1]), Path(sys.argv[2])
    result = {}
    for vdir in sorted(reports.iterdir()):
        jar = vdir / "server.jar"
        packets = vdir / "out/reports/packets.json"
        if not jar.exists():
            continue
        with zipfile.ZipFile(jar) as z:
            meta = json.loads(z.read("version.json"))
        proto = str(meta["protocol_version"])
        if packets.exists():
            entry = from_vanilla(json.loads(packets.read_text()))
        elif (mcdata / f"{vdir.name}.json").exists():
            entry = from_prismarine(json.loads((mcdata / f"{vdir.name}.json").read_text()))
        else:
            continue
        slot = result.setdefault(proto, {"versions": [], **entry})
        slot["versions"].append(meta["id"])
    for proto, entry in result.items():
        for key, names in WANTED.items():
            missing = [n for n in names if n not in entry[key]]
            expected_missing = {"start_configuration", "configuration_acknowledged",
                                "finish_configuration"} if int(proto) < 764 else set()
            bad = [n for n in missing if n not in expected_missing]
            if bad:
                sys.exit(f"protocol {proto}: missing {key} {bad}")
    ordered = {k: result[k] for k in sorted(result, key=int)}
    print(json.dumps(ordered, indent=1))


if __name__ == "__main__":
    main()
