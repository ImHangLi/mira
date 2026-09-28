"""A live Markdown page with a plain text editor, using only the standard library."""

import curses
import os
import re
import tempfile
import time
import unicodedata
from pathlib import Path

WELCOME = """# Project notes

This page shows NOTES.md in your project.

- Press **e** to edit, then **Esc** to save and read.
- Your agent can edit the file. This page updates at once.
"""


def put(screen, y, x, text, style=0):
    height, width = screen.getmaxyx()
    if 0 <= y < height and 0 <= x < width - 1:
        try:
            screen.addnstr(y, x, text, width - x - 1, style)
        except curses.error:
            pass


# Mira's palette in xterm 256 colors, the entries that read well on light and dark
# backgrounds: accent (terracotta), leaf (green), and key chips (dark ink on terracotta).
STYLE = {"accent": curses.A_BOLD, "leaf": curses.A_BOLD, "ochre": 0, "sky": 0, "chip": curses.A_REVERSE | curses.A_BOLD}


def init_palette():
    if not curses.has_colors():
        return
    try:
        curses.start_color()
        curses.use_default_colors()
        if curses.COLORS >= 256:
            curses.init_pair(1, 166, -1)
            curses.init_pair(2, 65, -1)
            curses.init_pair(3, 234, 209)
            curses.init_pair(4, 136, -1)
            curses.init_pair(5, 67, -1)
            STYLE.update(accent=curses.color_pair(1), leaf=curses.color_pair(2),
                         chip=curses.color_pair(3) | curses.A_BOLD,
                         ochre=curses.color_pair(4), sky=curses.color_pair(5))
    except curses.error:
        pass


def key_hints(screen, y, x, hints):
    """Whole key chips with labels, in the same order as Mira's footer."""
    width = screen.getmaxyx()[1]
    for key, label in hints:
        chip = f" {key} "
        text = f" {label}  "
        if x + len(chip) + len(text) >= width:
            break
        put(screen, y, x, chip, STYLE["chip"])
        x += len(chip)
        put(screen, y, x, text, curses.A_DIM)
        x += len(text)


def cell_width(char):
    if unicodedata.combining(char):
        return 0
    return 2 if unicodedata.east_asian_width(char) in ('W', 'F') else 1


def cells(text):
    return sum(cell_width(char) for char in text)


def inline(text, style=0):
    out = []
    for part in re.split(r'(\*\*[^*]+\*\*|\*[^*]+\*|`[^`]+`)', text):
        attr = style
        if part.startswith('**') and part.endswith('**'):
            part, attr = part[2:-2], style | curses.A_BOLD
        elif part.startswith('*') and part.endswith('*') and len(part) > 2:
            part, attr = part[1:-1], style | getattr(curses, 'A_ITALIC', curses.A_UNDERLINE)
        elif part.startswith('`') and part.endswith('`') and len(part) > 2:
            part, attr = part[1:-1], style | STYLE['ochre']
        out.extend((char, attr) for char in part)
    return out


def wrap(chars, width):
    """Wrap styled characters at spaces, splitting long words when needed."""
    rows = []
    while chars:
        used, end, space = 0, 0, 0
        for char, _ in chars:
            if used + cell_width(char) > width:
                break
            used += cell_width(char)
            end += 1
            if char == ' ':
                space = end
        end = max(1, end)
        if end < len(chars) and space:
            rows.append(chars[:space - 1])
            chars = chars[space:]
        else:
            rows.append(chars[:end])
            chars = chars[end:]
    return rows or [[]]


def markdown(text, width):
    rows, code = [], False
    for line in text.expandtabs(2).split('\n'):
        if line.strip().startswith('```'):
            code = not code
            continue
        prefix, style, rule = [], 0, False
        quote = line.startswith('> ') and not code
        if code:
            prefix = [(' ', 0)] * 2
            content = [(c, STYLE['ochre'] | curses.A_DIM) for c in line]
        elif line.strip() == '---':
            rows.append([('─', curses.A_DIM)] * width)
            continue
        else:
            heading = re.match(r'^(#{1,3})\s+(.*)', line)
            task = re.match(r'^[-*] \[([ xX])\] (.*)', line)
            number = re.match(r'^(\d+\.)\s+(.*)', line)
            if heading:
                line = heading[2]
                style = curses.A_BOLD | (STYLE['accent'] if len(heading[1]) == 1 else 0)
                rule = len(heading[1]) == 1
            elif task:
                checked = task[1].lower() == 'x'
                prefix = [(('☑' if checked else '☐'), STYLE['leaf'] if checked else STYLE['accent']), (' ', 0)]
                style = curses.A_DIM | STYLE['leaf'] if checked else 0
                line = task[2]
            elif line.startswith(('- ', '* ')):
                prefix, line = [('•', STYLE['accent']), (' ', 0)], line[2:]
            elif number:
                prefix = [(c, STYLE['accent']) for c in number[1] + ' ']
                line = number[2]
            elif line.startswith('> '):
                prefix, line = [('▌', STYLE['sky']), (' ', 0)], line[2:]
            content = inline(line, style)
        prefix = prefix[:max(0, width - 1)]
        for i, row in enumerate(wrap(content, max(1, width - len(prefix)))):
            lead = prefix if i == 0 or code or quote else [(' ', 0)] * len(prefix)
            rows.append(lead + row)
        if rule:
            rows.append([('─', curses.A_DIM)] * width)
    return rows


class Notes:
    def __init__(self):
        self.path = Path(os.environ.get('MIRA_WORKSPACE_ROOT') or os.getcwd()) / 'NOTES.md'
        self.mode = 'read'
        self.lines = ['']
        self.original = ''
        self.stamp = None
        self.saved_at = None
        self.updated_until = 0
        self.conflict = False
        self.error = ''
        self.row = self.col = self.top = self.scroll = 0
        self.load()

    def text(self):
        return '\n'.join(self.lines)

    def dirty(self):
        return self.text() != self.original

    def disk_stamp(self):
        try:
            stat = self.path.stat()
            return stat.st_mtime_ns, stat.st_size, stat.st_ino
        except FileNotFoundError:
            return None

    def load(self):
        try:
            try:
                with self.path.open(encoding='utf-8') as stream:
                    stamp = os.fstat(stream.fileno())
                    text = stream.read()
                self.stamp = (stamp.st_mtime_ns, stamp.st_size, stamp.st_ino)
                self.saved_at = stamp.st_mtime
            except FileNotFoundError:
                text, self.stamp, self.saved_at = WELCOME, None, None
            self.lines = text.split('\n')
            self.original = text
            self.row = min(self.row, len(self.lines) - 1)
            self.col = min(self.col, len(self.lines[self.row]))
            self.conflict, self.error = False, ''
            return True
        except (OSError, UnicodeError) as exc:
            self.error = f'Cannot read: {exc}'
            return False

    def poll(self):
        try:
            changed = self.disk_stamp() != self.stamp
            if changed:
                if self.mode == 'edit' and self.dirty():
                    self.conflict = True
                elif self.load():
                    self.updated_until = time.monotonic() + 2
            else:
                self.conflict = False
        except OSError as exc:
            self.error = f'Cannot read: {exc}'

    def save(self):
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(mode='w', encoding='utf-8', dir=self.path.parent,
                                             prefix='.NOTES.md-', delete=False) as stream:
                temporary = stream.name
                stream.write(self.text())
                stream.flush()
                os.fsync(stream.fileno())
                stat = os.fstat(stream.fileno())
            os.replace(temporary, self.path)
            self.stamp = (stat.st_mtime_ns, stat.st_size, stat.st_ino)
            self.saved_at = stat.st_mtime
            self.original = self.text()
            self.conflict, self.error = False, ''
            return True
        except OSError as exc:
            self.error = f'Cannot save: {exc}'
            return False
        finally:
            if temporary and os.path.exists(temporary):
                try:
                    os.unlink(temporary)
                except OSError:
                    pass

    def edit(self, key):
        line = self.lines[self.row]
        if key in ('\n', '\r', curses.KEY_ENTER):
            self.lines[self.row:self.row + 1] = [line[:self.col], line[self.col:]]
            self.row, self.col = self.row + 1, 0
        elif key in ('\b', '\x7f', curses.KEY_BACKSPACE):
            if self.col:
                self.lines[self.row] = line[:self.col - 1] + line[self.col:]
                self.col -= 1
            elif self.row:
                self.col = len(self.lines[self.row - 1])
                self.lines[self.row - 1] += self.lines.pop(self.row)
                self.row -= 1
        elif key == curses.KEY_DC:
            if self.col < len(line):
                self.lines[self.row] = line[:self.col] + line[self.col + 1:]
            elif self.row + 1 < len(self.lines):
                self.lines[self.row] += self.lines.pop(self.row + 1)
        elif key == curses.KEY_LEFT:
            if self.col:
                self.col -= 1
            elif self.row:
                self.row -= 1
                self.col = len(self.lines[self.row])
        elif key == curses.KEY_RIGHT:
            if self.col < len(line):
                self.col += 1
            elif self.row + 1 < len(self.lines):
                self.row, self.col = self.row + 1, 0
        elif key in (curses.KEY_UP, curses.KEY_DOWN):
            self.row = max(0, min(len(self.lines) - 1, self.row + (1 if key == curses.KEY_DOWN else -1)))
            self.col = min(self.col, len(self.lines[self.row]))
        elif key == curses.KEY_HOME:
            self.col = 0
        elif key == curses.KEY_END:
            self.col = len(line)
        elif isinstance(key, str) and (key.isprintable() or key == '\t'):
            text = '  ' if key == '\t' else key
            self.lines[self.row] = line[:self.col] + text + line[self.col:]
            self.col += len(text)


def draw(screen, notes):
    screen.erase()
    height, width = screen.getmaxyx()
    if width < 30 or height < 8:
        put(screen, 0, 0, 'NOTES.md')
        put(screen, 1, 0, 'Make the pane larger.', curses.A_DIM)
        screen.refresh()
        return
    status = 'not saved' if notes.dirty() or notes.saved_at is None else f'saved {max(0, int(time.time() - notes.saved_at))}s ago'
    label = f'NOTES.md  {status}'
    updated = time.monotonic() < notes.updated_until
    x = max(2, width - len(label) - (10 if updated else 2))
    put(screen, 0, x, label, curses.A_DIM)
    if updated:
        put(screen, 0, x + len(label) + 1, 'updated', STYLE['leaf'])
    x = 2
    for mode in ('read', 'edit'):
        label = f'  {mode.capitalize()}  '
        put(screen, 1, x, label, STYLE['chip'] if mode == notes.mode else curses.A_DIM)
        x += len(label) + 1
    room = height - 5
    if notes.mode == 'read':
        rows = markdown(notes.text(), width - 4)
        notes.scroll = max(0, min(notes.scroll, len(rows) - room))
        for y, row in enumerate(rows[notes.scroll:notes.scroll + room], 3):
            x = 2
            for char, style in row:
                put(screen, y, x, char, style)
                x += cell_width(char)
        hints = [('e', 'edit'), ('j/k', 'scroll'), ('q', 'quit')]
    else:
        notes.top = max(0, min(notes.top, notes.row))
        notes.top = max(notes.top, notes.row - room + 1)
        for y, line in enumerate(notes.lines[notes.top:notes.top + room], 3):
            x = 2
            for char in line:
                text = '  ' if char == '\t' else char if char.isprintable() else '�'
                if x + cells(text) > width - 2:
                    break
                put(screen, y, x, text)
                x += cells(text)
        hints = [('Esc', 'save and read'), ('Ctrl-S', 'save')]
    message = notes.error or ('changed on disk' if notes.conflict else '')
    put(screen, height - 2, 2, message, STYLE['ochre'])
    key_hints(screen, height - 1, 2, hints)
    if notes.mode == 'edit':
        x = cells(notes.lines[notes.row][:notes.col].expandtabs(2))
        screen.move(3 + notes.row - notes.top, min(width - 3, 2 + x))
    screen.refresh()


def main(screen):
    screen.keypad(True)
    screen.timeout(300)
    curses.set_escdelay(25)
    init_palette()
    notes = Notes()
    next_poll = 0
    while True:
        if time.monotonic() >= next_poll:
            notes.poll()
            next_poll = time.monotonic() + 0.3
        try:
            curses.curs_set(1 if notes.mode == 'edit' else 0)
        except curses.error:
            pass
        draw(screen, notes)
        if notes.mode == 'edit':
            try:
                key = screen.get_wch()
            except curses.error:
                continue
            if key == curses.KEY_RESIZE:
                screen.clear()
            if key in ('\x13', '\x1b'):
                # Nothing changed: Esc only goes back, so the welcome text never becomes a file.
                unchanged = not notes.dirty() and key == '\x1b'
                if (unchanged or notes.save()) and key == '\x1b':
                    notes.mode = 'read'
                    curses.noraw()
                    curses.cbreak()
            else:
                notes.edit(key)
        else:
            key = screen.getch()
            if key == curses.KEY_RESIZE:
                screen.clear()
            if key == ord('q'):
                return
            if key == ord('e'):
                notes.mode = 'edit'
                curses.raw()
            elif key in (ord('j'), curses.KEY_DOWN):
                notes.scroll += 1
            elif key in (ord('k'), curses.KEY_UP):
                notes.scroll -= 1
            elif key == ord('g'):
                notes.scroll = 0
            elif key == ord('G'):
                notes.scroll = len(markdown(notes.text(), max(1, screen.getmaxyx()[1] - 4)))


if __name__ == '__main__':
    curses.wrapper(main)
