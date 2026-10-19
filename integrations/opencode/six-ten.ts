// six-ten: coordinates edits between agents sharing this checkout. Installed by `six-ten install opencode`.
// One default export serves both plugin APIs: OpenCode 2 calls `setup`, OpenCode 1 calls `server`.

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

async function before(tool: string, sessionID: string, callID: string, args: unknown, directory: string) {
  const r = await sixTen({ event: "tool.execute.before", tool, sessionID, args, directory })
  if (r.code === 2) throw new Error(r.stderr.trim())
  const n = note(r.stdout)
  if (n) pending.set(callID, n)
}

/** The notes to append to this call's result, if any. */
async function after(tool: string, sessionID: string, callID: string, args: unknown, directory: string) {
  const r = await sixTen({ event: "tool.execute.after", tool, sessionID, args, directory })
  const notes = [turnNotes.get(sessionID), pending.get(callID), note(r.stdout)].filter(Boolean)
  turnNotes.delete(sessionID)
  pending.delete(callID)
  return notes.length ? notes.join("\n\n") : undefined
}

async function turnStart(sessionID: string, directory: string) {
  const n = note((await sixTen({ event: "chat.message", sessionID, directory })).stdout)
  if (n) turnNotes.set(sessionID, n)
}

/** A refused turn end (uncommitted work, or last agent out) comes back as text to prompt the session with. */
async function sessionEvent(type: string, sessionID: string, directory: string) {
  const r = await sixTen({ event: type, sessionID, directory })
  return r.code === 2 && r.stderr.trim() ? r.stderr.trim() : undefined
}

type V1Client = { session: { promptAsync(o: { path: { id: string }; body: { parts: { type: "text"; text: string }[] } }): Promise<unknown> } }

export default {
  id: "six-ten",
  async setup(ctx: any) {
    const directory: string = ctx.location.directory
    await ctx.tool.hook("execute.before", (e: any) => before(e.tool, e.sessionID, e.id, e.input, directory))
    await ctx.tool.hook("execute.after", async (e: any) => {
      const notes = await after(e.tool, e.sessionID, e.id, e.input, directory)
      if (!notes || e.status !== "completed") return
      const content = e.result.content
      const text = { type: "text", text: notes }
      e.result = { ...e.result, content: typeof content === "string" ? `${content}\n\n${notes}` : [...(content ?? []), text] }
    })
    await ctx.session.hook("prompt", (e: any) => turnStart(e.sessionID, directory))
    const controller = new AbortController()
    void (async () => {
      for await (const event of ctx.event.subscribe({ signal: controller.signal })) {
        if (event.type !== "session.idle" && event.type !== "session.deleted") continue
        const sessionID = event.data?.sessionID ?? event.data?.info?.id
        if (!sessionID) continue
        const text = await sessionEvent(event.type, sessionID, directory)
        if (text) await ctx.session.prompt({ sessionID, text })
      }
    })().catch(() => {})
    return () => controller.abort()
  },
  async server({ directory, client }: { directory: string; client: V1Client }) {
    return {
      "tool.execute.before": (input: { tool: string; sessionID: string; callID: string }, output: { args: unknown }) =>
        before(input.tool, input.sessionID, input.callID, output.args, directory),
      "tool.execute.after": async (input: { tool: string; sessionID: string; callID: string; args: unknown }, output: { output: string }) => {
        const notes = await after(input.tool, input.sessionID, input.callID, input.args, directory)
        if (notes) output.output = `${output.output}\n\n${notes}`
      },
      "chat.message": (input: { sessionID: string }) => turnStart(input.sessionID, directory),
      event: async ({ event }: { event: { type: string; properties?: any } }) => {
        if (event.type !== "session.idle" && event.type !== "session.deleted") return
        const sessionID = event.properties?.sessionID ?? event.properties?.info?.id
        if (!sessionID) return
        const text = await sessionEvent(event.type, sessionID, directory)
        if (text) await client.session.promptAsync({ path: { id: sessionID }, body: { parts: [{ type: "text", text }] } })
      },
    }
  },
}
