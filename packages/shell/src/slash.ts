/**
 * Slash command parser and registry.
 *
 * Returns the command name and args from a `/command arg1 arg2` string,
 * or null if it's not a slash command.
 */

export interface SlashCommand {
  name: string;
  description: string;
  execute: (args: string[], context: SlashContext) => Promise<void> | void;
}

export interface SlashContext {
  channelId: string | null;
  daemon: {
    channelSearch(query: string): Promise<{ results: unknown[] }>;
    canvasGet(channel: string): Promise<unknown>;
    channelList(): Promise<unknown[]>;
  };
  setNotice: (msg: string) => void;
  openSearch: (query: string) => void;
  toggleCanvas: () => void;
}

export function parseSlashCommand(
  input: string
): { name: string; args: string[] } | null {
  if (!input.startsWith("/")) return null;
  const parts = input.slice(1).split(/\s+/);
  const name = parts[0]?.toLowerCase();
  if (!name) return null;
  return { name, args: parts.slice(1) };
}

export const BUILTIN_COMMANDS: SlashCommand[] = [
  {
    name: "search",
    description: "Search messages — /search <query>",
    execute: (args, ctx) => {
      const query = args.join(" ");
      if (query) ctx.openSearch(query);
    },
  },
  {
    name: "canvas",
    description: "Toggle the channel canvas",
    execute: (_args, ctx) => {
      ctx.toggleCanvas();
    },
  },
  {
    name: "channels",
    description: "List channels",
    execute: async (_args, ctx) => {
      ctx.setNotice("Use Tab/Shift-Tab to switch channels");
    },
  },
  {
    name: "help",
    description: "Show available commands",
    execute: (_args, ctx) => {
      const lines = BUILTIN_COMMANDS.map(
        (cmd) => `/${cmd.name} — ${cmd.description}`
      ).join("\n");
      ctx.setNotice(lines);
    },
  },
];

export function findCommand(name: string): SlashCommand | undefined {
  return BUILTIN_COMMANDS.find((cmd) => cmd.name === name);
}
