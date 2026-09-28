"""A live system monitor in the spirit of btop: CPU, memory, disk, network, processes.

Standard library only. Reads /proc on Linux; on macOS it asks the kernel through
ctypes for CPU times and runs vm_stat, sysctl, netstat, and ps for the rest.
"""

import ctypes
import curses
import os
import platform
import shutil
import socket
import subprocess
import time
from collections import deque

MAC = platform.system() == "Darwin"
HISTORY = 400  # CPU and network samples kept for the graphs
BLOCKS = " ▁▂▃▄▅▆▇█"


def run(argv):
    try:
        return subprocess.run(argv, capture_output=True, text=True, timeout=3, check=False).stdout
    except (OSError, subprocess.TimeoutExpired):
        return ""


def sysctl(name):
    return run(["sysctl", "-n", name]).strip()


# ---------------------------------------------------------------- CPU

class _Mach:
    """host_processor_info: per-core ticks (user, system, idle, nice) on macOS."""

    def __init__(self):
        lib = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
        self.lib = lib
        lib.mach_host_self.restype = ctypes.c_uint
        lib.host_processor_info.argtypes = [
            ctypes.c_uint, ctypes.c_int, ctypes.POINTER(ctypes.c_uint),
            ctypes.POINTER(ctypes.POINTER(ctypes.c_int)), ctypes.POINTER(ctypes.c_uint)]
        lib.vm_deallocate.argtypes = [ctypes.c_uint, ctypes.c_void_p, ctypes.c_size_t]
        self.task = ctypes.c_uint.in_dll(lib, "mach_task_self_").value
        self.host = lib.mach_host_self()

    def ticks(self):
        ncpu, count = ctypes.c_uint(), ctypes.c_uint()
        info = ctypes.POINTER(ctypes.c_int)()
        if self.lib.host_processor_info(self.host, 2, ctypes.byref(ncpu), ctypes.byref(info),
                                        ctypes.byref(count)) != 0:
            return []
        out = []
        for c in range(ncpu.value):
            user, system, idle, nice = (info[c * 4 + i] & 0xFFFFFFFF for i in range(4))
            out.append((user + system + nice, user + system + idle + nice))
        self.lib.vm_deallocate(self.task, ctypes.cast(info, ctypes.c_void_p),
                               count.value * ctypes.sizeof(ctypes.c_int))
        return out


def _proc_ticks():
    out = []
    with open("/proc/stat", encoding="ascii") as f:
        for line in f:
            if line.startswith("cpu") and line[3].isdigit():
                v = [int(x) for x in line.split()[1:]]
                idle = v[3] + (v[4] if len(v) > 4 else 0)
                out.append((sum(v) - idle, sum(v)))
    return out


class Cpu:
    def __init__(self):
        self.mach = None
        if MAC:
            try:
                self.mach = _Mach()
            except (OSError, ValueError, AttributeError):
                pass
        self.prev = self.ticks()
        self.cores = [0.0] * len(self.prev)
        self.total = 0.0
        self.history = deque(maxlen=HISTORY)
        self.name = self.model()

    def ticks(self):
        try:
            return self.mach.ticks() if self.mach else _proc_ticks()
        except (OSError, ValueError):
            return []

    def model(self):
        if MAC:
            return sysctl("machdep.cpu.brand_string") or "CPU"
        try:
            with open("/proc/cpuinfo", encoding="utf-8") as f:
                for line in f:
                    if line.startswith("model name"):
                        return line.split(":", 1)[1].strip()
        except OSError:
            pass
        return "CPU"

    def sample(self):
        now = self.ticks()
        if len(now) == len(self.prev) and now:
            busy = total = 0
            for i, ((b1, t1), (b0, t0)) in enumerate(zip(now, self.prev)):
                db, dt = b1 - b0, t1 - t0
                self.cores[i] = db / dt if dt > 0 else 0.0
                busy, total = busy + db, total + dt
            self.total = busy / total if total > 0 else 0.0
        self.prev = now
        self.history.append(self.total)


# ---------------------------------------------------------------- memory, disk, network

def memory():
    """(total, used, cached, swap_total, swap_used) in bytes."""
    if MAC:
        total = int(sysctl("hw.memsize") or 0)
        pages, size = {}, 4096
        for line in run(["vm_stat"]).splitlines():
            if "page size of" in line:
                size = int(line.split("page size of")[1].split()[0])
            elif ":" in line:
                k, v = line.split(":", 1)
                try:
                    pages[k.strip()] = int(v.strip().rstrip("."))
                except ValueError:
                    pass
        page = lambda k: pages.get(k, 0) * size
        used = (page("Anonymous pages") - page("Pages purgeable") + page("Pages wired down")
                + page("Pages occupied by compressor"))
        cached = page("File-backed pages") + page("Pages purgeable")
        swap_total = swap_used = 0
        parts = sysctl("vm.swapusage").replace("=", " ").split()
        for i, p in enumerate(parts[:-1]):
            if p in ("total", "used"):
                n = _size(parts[i + 1])
                swap_total, swap_used = (n, swap_used) if p == "total" else (swap_total, n)
        return total, max(0, used), max(0, cached), swap_total, swap_used
    info = {}
    try:
        with open("/proc/meminfo", encoding="ascii") as f:
            for line in f:
                k, v = line.split(":", 1)
                info[k] = int(v.split()[0]) * 1024
    except (OSError, ValueError):
        return 0, 0, 0, 0, 0
    total = info.get("MemTotal", 0)
    cached = info.get("Cached", 0) + info.get("Buffers", 0)
    used = total - info.get("MemAvailable", info.get("MemFree", 0))
    return (total, used, cached, info.get("SwapTotal", 0),
            info.get("SwapTotal", 0) - info.get("SwapFree", 0))


def _size(text):
    units = {"K": 1 << 10, "M": 1 << 20, "G": 1 << 30, "T": 1 << 40}
    try:
        return int(float(text[:-1]) * units.get(text[-1].upper(), 1))
    except (ValueError, IndexError):
        return 0


def net_bytes():
    """Bytes received and sent over every interface but loopback."""
    rx = tx = 0
    if MAC:
        seen = set()
        for line in run(["netstat", "-ibn"]).splitlines()[1:]:
            f = line.split()
            if len(f) < 10 or f[0].startswith("lo") or "<Link" not in f[2] or f[0] in seen:
                continue
            seen.add(f[0])
            try:
                rx, tx = rx + int(f[-5]), tx + int(f[-2])
            except ValueError:
                pass
        return rx, tx
    try:
        with open("/proc/net/dev", encoding="ascii") as f:
            for line in f.readlines()[2:]:
                name, data = line.split(":", 1)
                if name.strip() == "lo":
                    continue
                v = data.split()
                rx, tx = rx + int(v[0]), tx + int(v[8])
    except (OSError, ValueError, IndexError):
        pass
    return rx, tx


def uptime():
    try:
        if MAC:
            boot = int(sysctl("kern.boottime").split("sec =")[1].split(",")[0])
            return time.time() - boot
        with open("/proc/uptime", encoding="ascii") as f:
            return float(f.read().split()[0])
    except (OSError, ValueError, IndexError):
        return 0


def processes():
    """(pid, name, user, cpu %, rss bytes) for every process."""
    out = []
    for line in run(["ps", "-Ao", "pid=,pcpu=,rss=,user=,comm="]).splitlines():
        f = line.split(None, 4)
        if len(f) < 5:
            continue
        try:
            out.append((int(f[0]), os.path.basename(f[4].strip()), f[3],
                        float(f[1].replace(",", ".")), int(f[2]) * 1024))
        except ValueError:
            pass
    return out


def human(n, rate=False):
    for unit in ("B", "K", "M", "G", "T"):
        if abs(n) < 1024 or unit == "T":
            text = f"{n:.0f}{unit}" if unit == "B" or n >= 100 else f"{n:.1f}{unit}"
            return text + ("/s" if rate else "")
        n /= 1024
    return str(n)


def span(secs):
    secs = int(secs)
    d, h, m = secs // 86400, secs // 3600 % 24, secs // 60 % 60
    return f"{d}d {h:02}:{m:02}" if d else f"{h:02}:{m:02}:{secs % 60:02}"


# ---------------------------------------------------------------- drawing

# Mira's palette in xterm 256 colors: accent (terracotta), leaf (green), ochre, sky, and
# key chips (dark ink on terracotta). Every one reads on light and dark backgrounds.
STYLE = {"accent": curses.A_BOLD, "leaf": 0, "ochre": 0, "sky": 0,
         "chip": curses.A_REVERSE | curses.A_BOLD, "sel": curses.A_REVERSE}


def init_palette():
    if not curses.has_colors():
        return
    try:
        curses.start_color()
        curses.use_default_colors()
        if curses.COLORS >= 256:
            for n, fg, bg in ((1, 166, -1), (2, 65, -1), (3, 234, 209), (4, 136, -1),
                              (5, 67, -1), (6, 234, 180)):
                curses.init_pair(n, fg, bg)
            STYLE.update(accent=curses.color_pair(1), leaf=curses.color_pair(2),
                         chip=curses.color_pair(3) | curses.A_BOLD, ochre=curses.color_pair(4),
                         sky=curses.color_pair(5), sel=curses.color_pair(6))
    except curses.error:
        pass


def heat(v):
    """Green when calm, ochre when busy, terracotta when hot."""
    return STYLE["leaf"] if v < 0.5 else STYLE["ochre"] if v < 0.8 else STYLE["accent"]


def put(screen, y, x, text, style=0):
    height, width = screen.getmaxyx()
    if 0 <= y < height and 0 <= x < width:
        try:
            # Writing the bottom-right cell raises after it draws; the text is on screen.
            screen.addnstr(y, x, text, width - x, style)
        except curses.error:
            pass


def box(screen, y, x, h, w, title, right=""):
    """A section in Mira's style: an uppercase title in the accent, a thin rule, and muted
    details on the right, like the sidebar's group titles. Returns the area below it."""
    if h < 2 or w < 8:
        return y, x, 0, 0
    name, _, value = title.partition(" ")
    put(screen, y, x + 1, name.upper(), STYLE["accent"] | curses.A_BOLD)
    col = x + 1 + len(name)
    if value:
        put(screen, y, col + 1, value, curses.A_BOLD)
        col += 1 + len(value)
    end = x + w - 1
    if right and col + len(right) + 6 < end:
        end -= len(right) + 1
        put(screen, y, end + 1, right, curses.A_DIM)
    if end - col > 3:
        put(screen, y, col + 1, "─" * (end - col - 2), curses.A_DIM)
    return y + 1, x + 1, h - 2, w - 2


def meter(screen, y, x, w, v, style=None):
    """A bar of small squares: filled in the heat color, the rest dotted."""
    v = max(0.0, min(1.0, v))
    n = round(v * w)
    put(screen, y, x, "■" * n, heat(v) if style is None else style)
    put(screen, y, x + n, "·" * (w - n), curses.A_DIM)


def graph(screen, y, x, h, w, values, style_of, top=1.0):
    """A filled area graph of the last `w` values, newest on the right."""
    vals = list(values)[-w:]
    vals = [0.0] * (w - len(vals)) + vals
    for col, v in enumerate(vals):
        eighths = int(round(max(0.0, min(1.0, v / top if top else 0)) * h * 8))
        for row in range(h):
            fill = max(0, min(8, eighths - row * 8))
            if fill:
                put(screen, y + h - 1 - row, x + col, BLOCKS[fill], style_of(row / max(1, h - 1)))


def key_hints(screen, y, x, hints):
    width = screen.getmaxyx()[1]
    for key, label in hints:
        chip, text = f" {key} ", f" {label}  "
        if x + len(chip) + len(text) >= width:
            break
        put(screen, y, x, chip, STYLE["chip"])
        put(screen, y, x + len(chip), text, curses.A_DIM)
        x += len(chip) + len(text)


class Monitor:
    def __init__(self):
        self.cpu = Cpu()
        self.host = socket.gethostname().split(".")[0]
        self.mem = memory()
        self.disk = shutil.disk_usage("/")
        self.net = net_bytes()
        self.net_at = time.monotonic()
        self.rx = deque(maxlen=HISTORY)
        self.tx = deque(maxlen=HISTORY)
        self.procs = processes()
        self.sort = "cpu"
        self.selected = 0
        self.up = uptime()

    def sample(self, full):
        """CPU and network every tick; memory, disk, and processes on `full` ticks."""
        self.cpu.sample()
        now, at = net_bytes(), time.monotonic()
        dt = max(0.001, at - self.net_at)
        self.rx.append(max(0, now[0] - self.net[0]) / dt)
        self.tx.append(max(0, now[1] - self.net[1]) / dt)
        self.net, self.net_at = now, at
        if full:
            self.mem = memory()
            self.disk = shutil.disk_usage("/")
            self.procs = processes()
            self.up = uptime()

    def sorted_procs(self):
        i = 3 if self.sort == "cpu" else 4
        return sorted(self.procs, key=lambda p: p[i], reverse=True)

    def draw(self, screen):
        screen.erase()
        height, width = screen.getmaxyx()
        if width < 40 or height < 14:
            put(screen, 0, 0, f"CPU {self.cpu.total:4.0%}", heat(self.cpu.total) | curses.A_BOLD)
            total, used = self.mem[0], self.mem[1]
            put(screen, 1, 0, f"MEM {human(used)} of {human(total)}")
            put(screen, 2, 0, "Make the pane larger for the full view.", curses.A_DIM)
            screen.refresh()
            return
        body_h = height - 1
        cpu_h = max(7, min(body_h * 2 // 5, len(self.cpu.cores) // max(1, (width - 4) // 2 // 22) + 4))
        self.draw_cpu(screen, 0, 0, cpu_h, width)
        rest = body_h - cpu_h
        mem_h = 6 + (1 if self.mem[3] else 0)
        if width >= 90:
            left_w = min(46, width * 2 // 5)
            self.draw_mem(screen, cpu_h, 0, mem_h, left_w)
            self.draw_net(screen, cpu_h + mem_h, 0, rest - mem_h, left_w)
            self.draw_procs(screen, cpu_h, left_w, rest, width - left_w)
        else:
            self.draw_mem(screen, cpu_h, 0, mem_h, width)
            self.draw_procs(screen, cpu_h + mem_h, 0, rest - mem_h, width)
        key_hints(screen, height - 1, 1, [("c", "sort by CPU"), ("m", "sort by memory"),
                                          ("↑↓", "move"), ("q", "quit")])
        screen.refresh()

    def draw_cpu(self, screen, y, x, h, w):
        load = " ".join(f"{v:.2f}" for v in os.getloadavg())
        y, x, h, w = box(screen, y, x, h, w, f"cpu {self.cpu.total:.0%}",
                         f"{self.host} · up {span(self.up)} · load {load}")
        if not h:
            return
        cores = self.cpu.cores
        # Enough columns that every core fits under the model name, in at most 3/5 of the width.
        cols = -(-len(cores) // max(1, h - 1)) if cores else 0
        cell = max(12, min(22, (w * 3 // 5) // max(1, cols)))
        cols = min(cols, (w * 3 // 5) // cell)
        rows_needed = -(-len(cores) // cols) if cols else 0
        grid_w = cols * cell
        graph_w = w - grid_w - 2 if cols else w
        graph(screen, y, x, h, graph_w, self.cpu.history, heat)
        if not cols:
            return
        gx = x + graph_w + 2
        put(screen, y, gx, self.cpu.name[:grid_w - 1], curses.A_BOLD)
        for i, v in enumerate(cores):
            r, c = i % max(1, rows_needed), i // max(1, rows_needed)
            if 1 + r >= h:
                continue
            cx = gx + c * cell
            put(screen, y + 1 + r, cx, f"C{i:<2}", curses.A_DIM)
            meter(screen, y + 1 + r, cx + 4, max(1, cell - 11), v)
            put(screen, y + 1 + r, cx + cell - 6, f"{v:4.0%}", heat(v))

    def draw_mem(self, screen, y, x, h, w):
        total, used, cached, swap_total, swap_used = self.mem
        y, x, h, w = box(screen, y, x, h, w, "memory", f"{human(total)} total")
        rows = [("used", used, total, None), ("cached", cached, total, STYLE["sky"]),
                ("free", max(0, total - used - cached), total, STYLE["leaf"])]
        if swap_total:
            rows.append(("swap", swap_used, swap_total, None))
        d = self.disk
        rows.append(("disk /", d.used, d.total, None))
        for i, (name, n, of, style) in enumerate(rows[:h]):
            v = n / of if of else 0
            put(screen, y + i, x, f"{name:<7}", curses.A_DIM)
            bar = max(4, min(40, w - 7 - 15))
            meter(screen, y + i, x + 7, bar, v, style)
            put(screen, y + i, x + 8 + bar, f"{human(n):>7} {v:4.0%}")

    def draw_net(self, screen, y, x, h, w):
        rx = self.rx[-1] if self.rx else 0
        tx = self.tx[-1] if self.tx else 0
        y, x, h, w = box(screen, y, x, h, w, "network", f"↓ {human(rx, True)}  ↑ {human(tx, True)}")
        if h < 2:
            return
        top = max(1.0, max(self.rx, default=0), max(self.tx, default=0))
        half = h // 2
        graph(screen, y, x, half, w, self.rx, lambda _: STYLE["sky"], top)
        graph(screen, y + half, x, h - half, w, self.tx, lambda _: STYLE["accent"], top)
        put(screen, y, x, "↓ in", STYLE["sky"] | curses.A_BOLD)
        put(screen, y + half, x, "↑ out", STYLE["accent"] | curses.A_BOLD)

    def draw_procs(self, screen, y, x, h, w):
        procs = self.sorted_procs()
        y, x, h, w = box(screen, y, x, h, w, "processes",
                         f"{len(procs)} · by {'CPU' if self.sort == 'cpu' else 'memory'}")
        if h < 2:
            return
        name_w = max(8, w - 36)
        head = f"{'PID':>7}  {'NAME':<{name_w}} {'USER':<9}{'CPU':>6} {'MEM':>7}"
        put(screen, y, x, head[:w], curses.A_DIM | curses.A_BOLD)
        room = h - 1
        self.selected = max(0, min(self.selected, len(procs) - 1))
        first = max(0, self.selected - room + 1)
        for r, (pid, name, user, cpu, rss) in enumerate(procs[first:first + room]):
            line = (f"{pid:>7}  {name[:name_w]:<{name_w}} {user[:8]:<9}"
                    f"{cpu:>5.1f}% {human(rss):>7}")
            style = STYLE["sel"] if first + r == self.selected else 0
            put(screen, y + 1 + r, x, line[:w].ljust(w), style)
            if style == 0:
                put(screen, y + 1 + r, x + 9 + name_w + 10, f"{cpu:>5.1f}%", heat(min(1.0, cpu / 100)))


def main(screen):
    try:
        curses.curs_set(0)
    except curses.error:
        pass
    screen.keypad(True)
    screen.timeout(100)
    curses.set_escdelay(25)
    init_palette()
    m = Monitor()
    tick, next_sample = 0, time.monotonic() + 0.5
    m.draw(screen)
    while True:
        key = screen.getch()
        if key == curses.KEY_RESIZE:
            screen.clear()  # repaint every cell; a resized terminal may keep old text
        dirty = key != -1
        if time.monotonic() >= next_sample:
            tick += 1
            m.sample(tick % 2 == 0)
            next_sample = time.monotonic() + 0.5
            dirty = True
        if key == ord("q"):
            return
        if key == ord("c"):
            m.sort, m.selected = "cpu", 0
        elif key == ord("m"):
            m.sort, m.selected = "mem", 0
        elif key in (curses.KEY_DOWN, ord("j")):
            m.selected += 1
        elif key in (curses.KEY_UP, ord("k")):
            m.selected = max(0, m.selected - 1)
        if dirty:
            m.draw(screen)


if __name__ == "__main__":
    curses.wrapper(main)
