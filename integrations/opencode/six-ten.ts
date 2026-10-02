// six-ten: coordinates edits between agents sharing this checkout. Installed by `six-ten install opencode`.

async function sixTen(payload: Record<string, unknown>): Promise<{ code: number; stdout: string; stderr: string }> {
  try {
    const proc = Bun.spawn(["six-ten", "hook", "opencode"], { stdin: "pipe", stdout: "pipe", stderr: "pipe" })
    proc.stdin.write(JSON.stringify(payload))
    proc.stdin.end()
    const [code, stdout, stderr] = await Promise.all([proc.exited, new Response(proc.stdout).text(), new Response(proc.stderr).text()])
    return { code, stdout, stderr }
  } catch {
    // six-ten not installed: never block the agent.
    return { code: 0, stdout: "", stderr: "" }
  }
}

function note(stdout: string): string | undefined {
  try {
    return JSON.parse(stdout).note
  } catch {
    return undefined
  }
}

// Notes are appended to a tool result so the model sees them: per call, or per session at turn start.
const pending = new Map<string, string>()
const turnNotes = new Map<string, string>()

type Client = { session: { promptAsync(o: { path: { id: string }; body: { parts: { type: "text"; text: string }[] } }): Promise<unknown> } }

export const SixTen = async ({ directory, client }: { directory: string; client: Client }) => ({
  "tool.execute.before": async (input: { tool: string; sessionID: string; callID: string }, output: { args: unknown }) => {
    const r = await sixTen({ event: "tool.execute.before", tool: input.tool, sessionID: input.sessionID, args: output.args, directory })
    if (r.code === 2) throw new Error(r.stderr.trim())
    const n = note(r.stdout)
    if (n) pending.set(input.callID, n)
  },
  "tool.execute.after": async (input: { tool: string; sessionID: string; callID: string; args: unknown }, output: { output: string }) => {
    const r = await sixTen({ event: "tool.execute.after", tool: input.tool, sessionID: input.sessionID, args: input.args, directory })
    const notes = [turnNotes.get(input.sessionID), pending.get(input.callID), note(r.stdout)].filter(Boolean)
    turnNotes.delete(input.sessionID)
    pending.delete(input.callID)
    if (notes.length) output.output = `${output.output}\n\n${notes.join("\n\n")}`
  },
  "chat.message": async (input: { sessionID: string }) => {
    const n = note((await sixTen({ event: "chat.message", sessionID: input.sessionID, directory })).stdout)
    if (n) turnNotes.set(input.sessionID, n)
  },
  event: async ({ event }: { event: { type: string; properties?: any } }) => {
    if (event.type === "session.idle" || event.type === "session.deleted") {
      const sessionID = event.properties?.sessionID ?? event.properties?.info?.id
      if (!sessionID) return
      const r = await sixTen({ event: event.type, sessionID, directory })
      // A refused turn end (uncommitted work, or last agent out) goes back to the session as a prompt.
      if (r.code === 2 && r.stderr.trim()) await client.session.promptAsync({ path: { id: sessionID }, body: { parts: [{ type: "text", text: r.stderr.trim() }] } })
    }
  },
})
