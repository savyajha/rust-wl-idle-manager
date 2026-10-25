# Helpers shared by the VM tests of the greeter compositor and the greeter: the pointer,
# through QMP input-send-event on the USB tablet, whose range covers the whole layout
# (LAYOUT, in logical pixels, which the test defines), and the outputs, read with QEMU's
# screendump of the virtio-gpu `gpu`.

def input_events(*events):
    assert machine.qmp_client is not None
    machine.qmp_client.send("input-send-event", {"events": list(events)})  # type: ignore

def move(x, y):
    """The pointer to (x, y) of the layout, through the absolute tablet."""
    def scaled(v, size):
        return round(v * 0x7fff / size)
    input_events(
        {"type": "abs", "data": {"axis": "x", "value": scaled(x, LAYOUT[0])}},
        {"type": "abs", "data": {"axis": "y", "value": scaled(y, LAYOUT[1])}},
    )

def click(x, y):
    """A click at (x, y), moving there from a pixel away: the tablet reports only changes, so
    a compositor started since the last click would not learn where the pointer is."""
    move(x + 1, y)
    move(x, y)
    for down in [True, False]:
        input_events({"type": "btn", "data": {"down": down, "button": "left"}})

def screen(head=0):
    """An output (head 0: Virtual-1), from QEMU's screendump: its width and its RGB bytes."""
    path = machine.out_dir / "screen.ppm"
    machine.send_monitor_command(f"screendump {path} gpu {head}")
    data = path.read_bytes()
    header = re.match(rb"P6\s+(\d+)\s+(\d+)\s+\d+\s", data)
    assert header is not None
    return int(header.group(1)), data[header.end():]

def screen_pixel(x, y, head=0):
    width, pixels = screen(head)
    i = (y * width + x) * 3
    return list(pixels[i:i + 3])

def wait_for_screen(check, what, x=512, y=384, head=0):
    with machine.nested(f"waiting for {what} on the screen"):
        retry(lambda _: check(screen_pixel(x, y, head)), 20)
