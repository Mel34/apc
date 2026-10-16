code = '''#!/usr/bin/env python3
import curses
import subprocess
from typing import List

class APCCommander:
    def __init__(self, stdscr):
        self.stdscr = stdscr
        self.active_pane = 0  # 0: Left (Installed), 1: Right (Repos)
        
        # Caches
        self.installed_pkgs: List[str] = []
        self.repo_pkgs: List[str] = []
        
        # State for Left Pane
        self.left_cursor = 0
        self.left_filter = ""
        self.left_tagged = set()
        
        # State for Right Pane
        self.right_cursor = 0
        self.right_filter = ""
        self.right_tagged = set()
        
        # Info Preview State
        self.info_mode = False 
        self.info_content: List[str] = []

        self._init_curses()
        self._load_packages()

    def _init_curses(self):
        curses.curs_set(0)
        curses.start_color()
        curses.use_default_colors()
        curses.init_pair(1, curses.COLOR_WHITE, curses.COLOR_BLUE)    # Main Pane
        curses.init_pair(2, curses.COLOR_BLACK, curses.COLOR_CYAN)    # Active Cursor
        curses.init_pair(3, curses.COLOR_YELLOW, curses.COLOR_BLUE)   # Tagged item
        curses.init_pair(4, curses.COLOR_BLACK, curses.COLOR_WHITE)   # Bottom Key Bar

    def _load_packages(self):
        try:
            out_q = subprocess.check_output(["pacman", "-Qq"], text=True)
            self.installed_pkgs = sorted(out_q.splitlines())
        except Exception:
            self.installed_pkgs = ["coreutils", "linux", "mesa", "systemd", "vim"]

        try:
            out_s = subprocess.check_output(["pacman", "-Slq"], text=True)
            self.repo_pkgs = sorted(list(set(out_s.splitlines())))
        except Exception:
            self.repo_pkgs = ["bash", "htop", "linux-zen", "neovim", "zsh"]

    def _get_pkg_info(self, pkg_name: str, installed: bool) -> List[str]:
        cmd = ["pacman", "-Qil" if installed else "-Si", pkg_name]
        try:
            res = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            return res.stdout.splitlines() if res.returncode == 0 else ["No info available."]
        except Exception as e:
            return [str(e)]

    def run_pacman_action(self, cmd_args: List[str]):
        """Temporarily suspend curses and spawn interactive pacman."""
        curses.def_prog_mode()
        curses.endwin()
        try:
            subprocess.run(cmd_args)
            input("\\nPress [ENTER] to return to apc...")
        finally:
            self.stdscr.refresh()
            curses.reset_prog_mode()
            self._load_packages()

    def draw_pane(self, win, title: str, items: List[str], cursor: int, 
                  filter_text: str, tagged: set, is_active: bool, is_info_target: bool):
        h, w = win.getmaxyx()
        if h < 3 or w < 10:
            return

        win.bkgd(" ", curses.color_pair(1))
        win.box()

        # Header & Search Indicator
        header = f" {title} "
        search = f"[ /{filter_text} ]" if filter_text else ""
        win.addstr(0, 2, header[:w - 4], curses.A_BOLD)
        if w - len(search) - 2 > len(header) + 4 and search:
            win.addstr(0, w - len(search) - 2, search)

        # Quick Info Mode in Opposing Pane
        if is_info_target and self.info_mode:
            info_tag = " [ Info View (ESC to close) ] "
            win.addstr(0, 2, info_tag[:w - 4], curses.A_BOLD | curses.A_STANDOUT)
            for idx, line in enumerate(self.info_content[:h - 2]):
                try:
                    win.addstr(idx + 1, 2, line[:w - 4])
                except curses.error:
                    pass
            win.noutrefresh()
            return

        # Filter items
        filtered = [p for p in items if filter_text.lower() in p.lower()]
        visible_lines = h - 2
        offset = max(0, cursor - visible_lines + 1) if cursor >= visible_lines else 0

        for i in range(visible_lines):
            idx = offset + i
            if idx >= len(filtered):
                break
            pkg = filtered[idx]
            tag = "*" if pkg in tagged else " "
            label = f" {tag} {pkg}"[:w - 3]

            try:
                if idx == cursor and is_active:
                    win.addstr(i + 1, 1, label.ljust(w - 2), curses.color_pair(2))
                elif pkg in tagged:
                    win.addstr(i + 1, 1, label.ljust(w - 2), curses.color_pair(3) | curses.A_BOLD)
                else:
                    win.addstr(i + 1, 1, label.ljust(w - 2), curses.color_pair(1))
            except curses.error:
                pass

        # Footer Status
        count_str = f" {len(filtered)} pkgs ({len(tagged)} sel) "
        if len(count_str) < w - 4:
            try:
                win.addstr(h - 1, w - len(count_str) - 2, count_str)
            except curses.error:
                pass

        win.noutrefresh()

    def draw_bottom_bar(self, h, w):
        if self.active_pane == 0:
            keys = " 1Help  2Reinst 3Info   4      5      6      7Search 8Delete 9SyncDB 10Quit"
        else:
            keys = " 1Help  2      3Info   4      5Inst   6      7Search 8      9SyncDB 10Quit"
        
        # Clip to w - 1 to protect the bottom-right corner cell
        bar_text = keys.ljust(w - 1)[:w - 1]
        try:
            self.stdscr.addstr(h - 1, 0, bar_text, curses.color_pair(4))
        except curses.error:
            pass

    def _update_info(self):
        if self.active_pane == 0 and self.installed_pkgs:
            filtered = [p for p in self.installed_pkgs if self.left_filter.lower() in p.lower()]
            if 0 <= self.left_cursor < len(filtered):
                self.info_content = self._get_pkg_info(filtered[self.left_cursor], installed=True)
        elif self.active_pane == 1 and self.repo_pkgs:
            filtered = [p for p in self.repo_pkgs if self.right_filter.lower() in p.lower()]
            if 0 <= self.right_cursor < len(filtered):
                self.info_content = self._get_pkg_info(filtered[self.right_cursor], installed=False)

    def main_loop(self):
        while True:
            h, w = self.stdscr.getmaxyx()
            self.stdscr.clear()

            if h < 8 or w < 40:
                self.stdscr.addstr(0, 0, "Terminal too small!")
                self.stdscr.refresh()
                key = self.stdscr.getch()
                if key in (ord('q'), 27):
                    break
                continue

            pane_w = w // 2
            pane_h = h - 1

            left_win = curses.newwin(pane_h, pane_w, 0, 0)
            right_win = curses.newwin(pane_h, w - pane_w, 0, pane_w)

            # Left Pane (Installed)
            self.draw_pane(left_win, "Installed Packages", self.installed_pkgs, 
                           self.left_cursor, self.left_filter, self.left_tagged,
                           is_active=(self.active_pane == 0),
                           is_info_target=(self.active_pane == 1))

            # Right Pane (Repos)
            self.draw_pane(right_win, "Repository Packages", self.repo_pkgs, 
                           self.right_cursor, self.right_filter, self.right_tagged,
                           is_active=(self.active_pane == 1),
                           is_info_target=(self.active_pane == 0))

            self.draw_bottom_bar(h, w)
            curses.doupdate()

            key = self.stdscr.getch()

            if key in (ord('\\t'), 9):  # TAB
                self.active_pane = 1 - self.active_pane
                if self.info_mode:
                    self.info_mode = False

            elif key == 27:  # ESC
                self.info_mode = False

            elif key in (curses.KEY_UP, ord('k')):
                if self.active_pane == 0 and self.left_cursor > 0:
                    self.left_cursor -= 1
                elif self.active_pane == 1 and self.right_cursor > 0:
                    self.right_cursor -= 1
                if self.info_mode:
                    self._update_info()

            elif key in (curses.KEY_DOWN, ord('j')):
                if self.active_pane == 0 and self.left_cursor < len(self.installed_pkgs) - 1:
                    self.left_cursor += 1
                elif self.active_pane == 1 and self.right_cursor < len(self.repo_pkgs) - 1:
                    self.right_cursor += 1
                if self.info_mode:
                    self._update_info()

            elif key in (curses.KEY_F3, ord('3')):
                self.info_mode = not self.info_mode
                if self.info_mode:
                    self._update_info()

            elif key in (curses.KEY_F8, ord('8')):  # Delete
                if self.active_pane == 0 and self.installed_pkgs:
                    pkg = self.installed_pkgs[self.left_cursor]
                    self.run_pacman_action(["sudo", "pacman", "-Rns", pkg])

            elif key in (curses.KEY_F5, ord('5')):  # Install / Reinstall
                if self.active_pane == 1 and self.repo_pkgs:
                    pkg = self.repo_pkgs[self.right_cursor]
                    self.run_pacman_action(["sudo", "pacman", "-S", pkg])
                elif self.active_pane == 0 and self.installed_pkgs:
                    pkg = self.installed_pkgs[self.left_cursor]
                    self.run_pacman_action(["sudo", "pacman", "-S", pkg])

            elif key in (curses.KEY_F9, ord('9')):  # Sync DB
                self.run_pacman_action(["sudo", "pacman", "-Sy"])

            elif key in (curses.KEY_F10, ord('q'), ord('Q')):
                break

def main():
    curses.wrapper(lambda stdscr: APCCommander(stdscr).main_loop())

if __name__ == "__main__":
    main()
'''
import ast
ast.parse(code)
print("Syntax check passed cleanly!")
