"""A small curses game with two queued turns and a shared high score."""

import curses
import fcntl
import json
import os
import random
import time
from collections import deque
from pathlib import Path

# The board fits the window when a game starts: 12x6 to 40x20 cells, two columns each.
MIN_BOARD, MAX_BOARD = (12, 6), (40, 20)
CHROME = 8  # rows around the board: title, borders, and the three lines below


def board_size(screen):
    height, width = screen.getmaxyx()
    return (max(MIN_BOARD[0], min(MAX_BOARD[0], (width - 4) // 2)),
            max(MIN_BOARD[1], min(MAX_BOARD[1], height - CHROME)))


def high_score(score=0):
    path = Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local/state") / "mira/snake.json"
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("a+", encoding="utf-8") as stream:
            fcntl.flock(stream, fcntl.LOCK_EX)
            stream.seek(0)
            try:
                best = max(0, int(json.load(stream)["high_score"]))
            except (ValueError, KeyError, TypeError):
                best = 0
            if score > best:
                best = score
                stream.seek(0)
                stream.truncate()
                json.dump({"high_score": best}, stream)
                stream.flush()
            return best, ""
    except OSError:
        return score, "High score could not be saved."


class Game:
    def __init__(self, size):
        self.width, self.height = size
        x, y = self.width // 2, self.height // 2
        self.snake = deque([(x, y), (x - 1, y), (x - 2, y)])
        self.direction = (1, 0)
        self.turns = deque(maxlen=2)
        self.food = (x + 4, y)
        self.score = 0
        self.started = False
        self.paused = False
        self.over = False
        self.won = False
        self.bit_self = False

    @property
    def interval(self):
        return max(0.065, 0.18 - self.score * 0.006)

    def turn(self, direction):
        if not self.started:
            # The first key sets the heading; a reverse turns the snake around.
            if direction == (-self.direction[0], -self.direction[1]):
                self.snake.reverse()
            self.direction = direction
            self.started = True
            return
        previous = self.turns[-1] if self.turns else self.direction
        if len(self.turns) < 2 and direction != previous and direction != (-previous[0], -previous[1]):
            self.turns.append(direction)

    def step(self):
        if self.turns:
            self.direction = self.turns.popleft()
        x, y = self.snake[0]
        dx, dy = self.direction
        head = (x + dx, y + dy)
        eating = head == self.food
        body = self.snake if eating else list(self.snake)[:-1]
        if not (0 <= head[0] < self.width and 0 <= head[1] < self.height) or head in body:
            self.over = True
            self.bit_self = head in body
            return False
        self.snake.appendleft(head)
        if not eating:
            self.snake.pop()
            return False
        self.score += 1
        free = [(x, y) for y in range(self.height) for x in range(self.width)
                if (x, y) not in self.snake]
        if free:
            self.food = random.choice(free)
        else:
            self.food = None
            self.over = self.won = True
        return True


def put(screen, y, x, text, style=0):
    height, width = screen.getmaxyx()
    if 0 <= y < height and 0 <= x < width - 1:
        try:
            screen.addnstr(y, x, text, width - x - 1, style)
        except curses.error:
            # A resize can arrive between getmaxyx and addnstr.
            pass


# Mira's palette in xterm 256 colors, the entries that read well on light and dark
# backgrounds: accent (terracotta), leaf (green), and key chips (dark ink on terracotta).
STYLE = {"accent": curses.A_BOLD, "leaf": curses.A_BOLD, "chip": curses.A_REVERSE | curses.A_BOLD}


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
            STYLE.update(accent=curses.color_pair(1) | curses.A_BOLD,
                         leaf=curses.color_pair(2) | curses.A_BOLD,
                         chip=curses.color_pair(3) | curses.A_BOLD)
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


def draw(screen, game, best, warning):
    screen.erase()
    height, width = screen.getmaxyx()
    need_w, need_h = game.width * 2 + 2, game.height + CHROME
    # The last column cannot be drawn (see put), so the board needs one spare column.
    if width <= need_w or height < need_h:
        put(screen, 0, 0, "Make the window bigger")
        put(screen, 1, 0, f"Needs {need_w + 1} x {need_h}.")
        put(screen, 2, 0, "Game paused.")
        key_hints(screen, 4, 0, [("q", "quit")])
        screen.refresh()
        return False
    bw, bh = game.width, game.height
    left = (width - need_w) // 2
    top = (height - need_h) // 2
    put(screen, top, left, f"Score {game.score}   Best {best}", curses.A_BOLD)
    put(screen, top + 2, left, "┌" + "─" * (bw * 2) + "┐")
    for y in range(bh):
        put(screen, top + 3 + y, left, "│")
        put(screen, top + 3 + y, left + bw * 2 + 1, "│")
    put(screen, top + 3 + bh, left, "└" + "─" * (bw * 2) + "┘")
    for index, (x, y) in enumerate(game.snake):
        put(screen, top + 3 + y, left + 1 + x * 2, "██" if index == 0 else "▓▓", STYLE["leaf"])
    if game.food:
        x, y = game.food
        put(screen, top + 3 + y, left + 1 + x * 2, "● ", STYLE["accent"])
    if game.over:
        snacks = "snack" if game.score == 1 else "snacks"
        message = ("You filled the board. Take the rest of today off." if game.won else
                   f"Game over. {game.score} {snacks}. " +
                   ("You were the snack." if game.bit_self else "The wall was not one."))
    elif not game.started:
        message = "Press a direction to start. Snacks can wait."
    elif game.paused:
        message = "Paused. Your snacks are safe."
    else:
        message = "One snack at a time."
    put(screen, top + bh + 4, left, message)
    controls = ([("r", "restart"), ("q", "quit")] if game.over else
                [("Arrows/WASD", "move"), ("p", "pause"), ("q", "quit")])
    key_hints(screen, top + bh + 5, left, controls)
    if warning:
        put(screen, top + bh + 7, left, warning)
    screen.refresh()
    return True


def main(screen):
    try:
        curses.curs_set(0)
    except curses.error:
        pass
    screen.keypad(True)
    screen.timeout(20)
    curses.set_escdelay(25)
    init_palette()
    directions = {curses.KEY_UP: (0, -1), ord("w"): (0, -1),
                  curses.KEY_DOWN: (0, 1), ord("s"): (0, 1),
                  curses.KEY_LEFT: (-1, 0), ord("a"): (-1, 0),
                  curses.KEY_RIGHT: (1, 0), ord("d"): (1, 0)}
    game = Game(board_size(screen))
    best, warning = high_score()
    deadline = time.monotonic() + game.interval
    while True:
        # Until the first move, the board follows the window size.
        if not game.started and board_size(screen) != (game.width, game.height):
            game = Game(board_size(screen))
        fits = draw(screen, game, best, warning)
        key = screen.getch()
        if key == curses.KEY_RESIZE:
            screen.clear()  # repaint every cell; a resized terminal may keep old text
        if key == ord("q"):
            return
        if key == ord("r") and game.over:
            game = Game(board_size(screen))
            deadline = time.monotonic() + game.interval
        elif key == ord("p") and game.started and not game.over:
            game.paused = not game.paused
            game.turns.clear()
            deadline = time.monotonic() + game.interval
        elif key in directions and fits and not game.paused and not game.over:
            game.turn(directions[key])
        if not fits or not game.started or game.paused or game.over:
            deadline = time.monotonic() + game.interval
        elif time.monotonic() >= deadline:
            if game.step():
                saved, warning = high_score(game.score)
                best = max(best, saved)
            # Do not catch up in a burst after a slow terminal or a resize.
            deadline = time.monotonic() + game.interval


if __name__ == "__main__":
    curses.wrapper(main)
