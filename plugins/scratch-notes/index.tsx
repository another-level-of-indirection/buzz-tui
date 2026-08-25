/**
 * scratch-notes plugin — a local notepad pane.
 *
 * Notes persist to ~/.config/buzz-tui/scratch-notes.txt.
 * Demonstrates file I/O, slash commands, and pane rendering.
 */

import React, { useState, useEffect } from "react";
import { Box, Text } from "ink";
import { readFileSync, writeFileSync, mkdirSync, existsSync } from "fs";
import { join } from "path";
import { homedir } from "os";
import type { PluginFactory, BuzzPluginAPI } from "../../packages/plugin-sdk/index.ts";

const NOTES_DIR = join(homedir(), ".config", "buzz-tui");
const NOTES_FILE = join(NOTES_DIR, "scratch-notes.txt");

function loadNotes(): string[] {
  try {
    return readFileSync(NOTES_FILE, "utf-8").split("\n").filter(Boolean);
  } catch {
    return [];
  }
}

function saveNotes(notes: string[]) {
  if (!existsSync(NOTES_DIR)) {
    mkdirSync(NOTES_DIR, { recursive: true });
  }
  writeFileSync(NOTES_FILE, notes.join("\n") + "\n");
}

let notesState: string[] = [];
let refreshFn: (() => void) | null = null;

function NotesPane() {
  const [notes, setNotes] = useState(notesState);

  useEffect(() => {
    refreshFn = () => setNotes([...notesState]);
    return () => {
      refreshFn = null;
    };
  }, []);

  return (
    <Box flexDirection="column" borderStyle="single" borderColor="yellow" width={36}>
      <Box paddingX={1}>
        <Text bold color="yellow">
          Scratch Notes ({notes.length})
        </Text>
      </Box>
      <Box flexDirection="column" paddingX={1}>
        {notes.length === 0 ? (
          <Text color="gray" italic>
            No notes yet. Use /note &lt;text&gt;
          </Text>
        ) : (
          notes.slice(-20).map((note, i) => (
            <Box key={i}>
              <Text color="gray" dimColor>
                {(notes.length - 20 + i + 1).toString().padStart(2)}{" "}
              </Text>
              <Text>{note}</Text>
            </Box>
          ))
        )}
      </Box>
    </Box>
  );
}

const factory: PluginFactory = () => ({
  activate(api: BuzzPluginAPI) {
    notesState = loadNotes();
    let visible = false;

    function toggle() {
      visible = !visible;
      api.ui.setPane("scratch-notes", visible ? <NotesPane /> : null);
    }

    api.ui.registerCommand("notes", () => toggle());
    api.ui.registerCommand("note", (args) => {
      const text = args.join(" ").trim();
      if (!text) {
        api.ui.showNotice("Usage: /note <text>");
        return;
      }
      const timestamp = new Date().toLocaleTimeString([], {
        hour: "2-digit",
        minute: "2-digit",
      });
      notesState.push(`[${timestamp}] ${text}`);
      saveNotes(notesState);
      refreshFn?.();
      api.ui.showNotice(`Note saved (${notesState.length} total)`);
    });

    api.ui.showNotice("Scratch notes: /notes to toggle, /note <text> to add");

    return () => {
      api.ui.setPane("scratch-notes", null);
    };
  },
});

export default factory;
