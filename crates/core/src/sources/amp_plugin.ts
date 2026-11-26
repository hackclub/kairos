import type { PluginAPI } from '@ampcode/plugin'
import { spawn } from 'node:child_process'
import { basename } from 'node:path'

export const description = 'Lets Kairos track your focus time in Amp.'

const KAIROS = '__KAIROS__'

function send(event: object) {
	try {
		const p = spawn(KAIROS, ['amp-event'], { stdio: ['pipe', 'ignore', 'ignore'], windowsHide: true })
		p.on('error', () => {})
		p.stdin.on('error', () => {})
		p.stdin.end(JSON.stringify(event))
	} catch {}
}

export default function (amp: PluginAPI) {
	const typed = new Set<string>()
	const working = new Set<string>()
	let version: string | undefined
	if (/^amp(\.exe)?$/i.test(basename(process.execPath))) {
		amp.$`${process.execPath} --version`.then(
			r => (version = r.stdout.trim().split(/\s+/)[0] || undefined),
			() => {},
		)
	}
	const base = (thread: string) => {
		const root = amp.system.workspaceRoot
		return { ts: Date.now() / 1000, thread, root: root ? amp.helpers.filePathFromURI(root) : null }
	}

	amp.on('agent.start', async event => {
		const id = event.thread.id
		working.delete(id)
		// A prompt counts when its thread is the one on screen: `amp -x` and subagent threads have none.
		if (event.changesWorkflow || amp.activeThread.current?.id !== id) {
			typed.delete(id)
			return {}
		}
		typed.add(id)
		send({ ev: 'prompt', ...base(id), text: event.message, version })
		return {}
	})

	amp.on('tool.result', async event => {
		const id = event.thread.id
		// Like the other harnesses, the agent counts as working from its first tool.
		if (typed.has(id) && !working.has(id)) {
			working.add(id)
			send({ ev: 'start', ...base(id) })
		}
		if (event.status !== 'done') return
		const commit = event.tool === 'shell_command' && String(event.input.command).includes('commit')
		if (commit || event.tool === 'apply_patch' || event.tool === 'edit_file') {
			send({ ev: 'tool', ...base(id), tool: event.tool, input: event.input, output: event.output })
		}
	})

	amp.on('agent.end', async event => {
		const id = event.thread.id
		working.delete(id)
		if (!typed.delete(id)) return
		const reply: string[] = []
		let tools = 0
		let model: string | undefined
		for (const m of event.messages) {
			if (m.role !== 'assistant') continue
			model = m.usage?.model ?? model
			for (const b of m.content) {
				if (b.type === 'text') reply.push(b.text)
				if (b.type === 'tool_use') tools++
			}
		}
		const files = new Set<string>()
		for (const call of amp.helpers.toolCallsInMessages(event.messages)) {
			if (call.result?.status !== 'done') continue
			for (const uri of amp.helpers.filesModifiedByToolCall(call.result) ?? []) files.add(String(uri))
		}
		send({ ev: 'end', ...base(id), reply, tools, files: files.size, model })
	})
}
