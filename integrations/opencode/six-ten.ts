// six-ten: blocks edits to files another agent is editing. Installed by `six-ten install opencode`.

async function sixTen(payload: Record<string, unknown>): Promise<{ code: number; stderr: string }> {
  try {
    const proc = Bun.spawn(["six-ten", "hook", "opencode"], { stdin: "pipe", stdout: "ignore", stderr: "pipe" })
    proc.stdin.write(JSON.stringify(payload))
    proc.stdin.end()
    const [code, stderr] = await Promise.all([proc.exited, new Response(proc.stderr).text()])
    return { code, stderr }
  } catch {
    // six-ten not installed: never block the agent.
    return { code: 0, stderr: "" }
  }
}

export const SixTen = async ({ directory }: { directory: string }) => ({
  "tool.execute.before": async (input: { tool: string; sessionID: string }, output: { args: unknown }) => {
    const { code, stderr } = await sixTen({ event: "tool.execute.before", tool: input.tool, sessionID: input.sessionID, args: output.args, directory })
    if (code === 2) throw new Error(stderr.trim())
  },
  event: async ({ event }: { event: { type: string; properties?: any } }) => {
    if (event.type === "session.idle" || event.type === "session.deleted") {
      const sessionID = event.properties?.sessionID ?? event.properties?.info?.id
      if (sessionID) await sixTen({ event: event.type, sessionID, directory })
    }
  },
})
