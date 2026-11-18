import { useState } from 'react'
import type { AnswerMode, InterviewLanguage, JobContext, Settings, Speaker } from './copilotTypes'
import { ANSWER_MODE_LABELS, modelChoices, parseTopics } from './suggestionView'
import type { CopilotApi } from './useCopilot'

type Props = {
  copilot: CopilotApi
  language: InterviewLanguage
  onTestTurn: (speaker: Speaker, text: string) => Promise<void>
}

const field = 'mt-1 w-full rounded-lg border border-white/10 bg-black/30 px-3 py-2 text-sm text-stone-200'
const label = 'mt-4 block text-xs uppercase tracking-wider text-stone-500'
const button = 'mt-2 w-full rounded-lg border border-white/10 px-4 py-2 text-sm text-stone-300 disabled:opacity-40'

export function CopilotSetup({ copilot, language, onTestTurn }: Props) {
  const { status } = copilot
  return (
    <div className="mt-5 rounded-2xl border border-white/10 bg-[#11151b] p-5">
      <h2 className="text-lg font-medium">Interview copilot</h2>
      {copilot.error && <p role="alert" className="mt-3 rounded-lg bg-red-400/10 p-3 text-xs leading-5 text-red-100">{copilot.error}</p>}
      {status?.settingsWarning && <p className="mt-3 rounded-lg bg-amber-300/10 p-3 text-xs leading-5 text-amber-100">{status.settingsWarning}</p>}
      {status ? (
        <>
          <ModelSection copilot={copilot} settings={status.settings} />
          <AnswerSection copilot={copilot} settings={status.settings} />
          <ProfileSection copilot={copilot} initialPath={status.profilePath} />
          <JobSection copilot={copilot} language={language} initial={status.settings.job} />
          <MemorySection copilot={copilot} />
        </>
      ) : (
        <p className="mt-5 text-xs text-stone-500">Loading copilot state…</p>
      )}
      <TestTurnSection copilot={copilot} onTestTurn={onTestTurn} />
    </div>
  )
}

function ModelSection({ copilot, settings }: { copilot: CopilotApi; settings: Settings }) {
  const accounts = copilot.status?.accounts ?? []
  const [accountEmail, setAccountEmail] = useState(settings.accountEmail ?? '')
  const [model, setModel] = useState(settings.model)
  const [reasoning, setReasoning] = useState(settings.reasoningEffort)
  const [screenModel, setScreenModel] = useState(settings.screenModel)
  const models = modelChoices(copilot.connection?.models ?? [], [model, screenModel])
  const savedEmailMissing = accountEmail !== '' && !accounts.some((account) => account.email === accountEmail)
  const connection = copilot.connection
  const dirty =
    accountEmail !== (settings.accountEmail ?? '') || model !== settings.model || reasoning !== settings.reasoningEffort || screenModel !== settings.screenModel

  return (
    <>
      <h3 className="mt-5 text-sm font-semibold text-stone-300">ChatGPT</h3>
      <p className={`mt-2 rounded-lg p-3 text-xs leading-5 ${connection?.ok ? 'bg-emerald-400/10 text-emerald-100' : 'bg-amber-300/10 text-amber-100'}`}>
        {copilot.checking ? 'Checking the ChatGPT connection…' : (connection?.message ?? 'Not checked yet.')}
      </p>
      <label className={label} htmlFor="account">Account (your existing Codex sign-in)</label>
      <select id="account" value={accountEmail} onChange={(event) => setAccountEmail(event.target.value)} className={field}>
        <option value="">Choose account…</option>
        {savedEmailMissing && <option value={accountEmail}>{accountEmail} — not signed in on this Mac</option>}
        {accounts.map((account) => (
          <option key={account.email} value={account.email}>
            {account.email} · {account.plan ?? 'plan?'}{account.expired ? ' · expired' : ''} · {account.source}
          </option>
        ))}
      </select>
      <label className={label} htmlFor="model">Answer model</label>
      <select id="model" value={model} onChange={(event) => setModel(event.target.value)} className={field}>
        {models.map((slug) => <option key={slug} value={slug}>{slug}</option>)}
      </select>
      <label className={label} htmlFor="reasoning">Reasoning</label>
      <select id="reasoning" value={reasoning} onChange={(event) => setReasoning(event.target.value)} className={field}>
        <option value="none">none — fastest</option>
        <option value="low">low</option>
        <option value="medium">medium — slower</option>
        <option value="">model default</option>
      </select>
      <label className={label} htmlFor="screen-model">Model for “Låt oss se”</label>
      <select id="screen-model" value={screenModel} onChange={(event) => setScreenModel(event.target.value)} className={field}>
        <option value="">Same as answer model</option>
        {models.map((slug) => <option key={slug} value={slug}>{slug}</option>)}
      </select>
      <div className="mt-2 flex gap-2">
        <button
          type="button"
          disabled={!dirty || !model}
          onClick={() => void copilot.saveModelSettings({ accountEmail: accountEmail || null, model, reasoningEffort: reasoning, screenModel })}
          className="flex-1 rounded-lg bg-emerald-300 px-3 py-2 text-sm font-semibold text-emerald-950 disabled:opacity-40"
        >
          Save
        </button>
        <button type="button" disabled={copilot.checking} onClick={() => void copilot.checkConnection()} className="flex-1 rounded-lg border border-white/10 px-3 py-2 text-sm text-stone-300 disabled:opacity-40">
          Check connection
        </button>
      </div>
    </>
  )
}

function AnswerSection({ copilot, settings }: { copilot: CopilotApi; settings: Settings }) {
  return (
    <>
      <h3 className="mt-6 text-sm font-semibold text-stone-300">Answers</h3>
      <label className="mt-3 flex items-center gap-2 text-sm text-stone-300">
        <input type="checkbox" checked={settings.autoAnswer} onChange={(event) => void copilot.setAutoAnswer(event.target.checked)} />
        Answer automatically when the interviewer finishes
      </label>
      <label className="mt-3 flex items-center gap-2 text-sm text-stone-300">
        <input type="checkbox" checked={settings.historyEnabled} onChange={(event) => void copilot.setHistoryEnabled(event.target.checked)} />
        Save interviews to local history (this Mac only)
      </label>
      {copilot.status?.historyError && <p className="mt-1 text-xs text-amber-200/80">History: {copilot.status.historyError}</p>}
      <label className={label} htmlFor="answer-mode">Default answer style</label>
      <select id="answer-mode" value={settings.answerMode} onChange={(event) => void copilot.setAnswerMode(event.target.value as AnswerMode)} className={field}>
        {(Object.keys(ANSWER_MODE_LABELS) as AnswerMode[]).map((mode) => <option key={mode} value={mode}>{ANSWER_MODE_LABELS[mode]}</option>)}
      </select>
    </>
  )
}

function ProfileSection({ copilot, initialPath }: { copilot: CopilotApi; initialPath: string }) {
  const [profilePath, setProfilePath] = useState(initialPath)
  const profile = copilot.status?.profile
  return (
    <>
      <h3 className="mt-6 text-sm font-semibold text-stone-300">Candidate profile</h3>
      <label className={label} htmlFor="profile-path">profile.yaml path</label>
      <input id="profile-path" value={profilePath} onChange={(event) => setProfilePath(event.target.value)} className={field} spellCheck={false} />
      <button type="button" onClick={() => void copilot.reloadProfile(profilePath)} className={button}>
        Load profile
      </button>
      {profile ? (
        <div className="mt-2 rounded-lg bg-black/25 p-3 text-xs leading-5 text-stone-400">
          <p className="text-stone-200">{profile.candidateName ?? 'Unnamed candidate'} · {profile.stories.length} stories · {profile.projects.length} projects</p>
          {profile.warnings.map((warning) => <p key={warning} className="text-amber-200/80">⚠ {warning}</p>)}
        </div>
      ) : (
        <p className="mt-2 rounded-lg bg-amber-300/10 p-3 text-xs leading-5 text-amber-100">
          {copilot.status?.profileError ?? 'No profile loaded.'} Copy profile.example.yaml to profile.yaml and fill in your real experience. Without a profile, answers make no personal claims.
        </p>
      )}
    </>
  )
}

function JobSection({ copilot, language, initial }: { copilot: CopilotApi; language: InterviewLanguage; initial: JobContext }) {
  const [company, setCompany] = useState(initial.company)
  const [role, setRole] = useState(initial.role)
  const [topics, setTopics] = useState(initial.importantTopics.join(', '))
  const [description, setDescription] = useState(initial.jobDescription)
  const [saved, setSaved] = useState(false)
  const edit = (setter: (value: string) => void) => (value: string) => {
    setter(value)
    setSaved(false)
  }

  async function save() {
    await copilot.applyJob({ company, role, jobDescription: description, importantTopics: parseTopics(topics), language })
    setSaved(true)
  }

  return (
    <>
      <h3 className="mt-6 text-sm font-semibold text-stone-300">Job (saved between launches)</h3>
      <label className={label} htmlFor="job-company">Company</label>
      <input id="job-company" value={company} onChange={(event) => edit(setCompany)(event.target.value)} className={field} />
      <label className={label} htmlFor="job-role">Role</label>
      <input id="job-role" value={role} onChange={(event) => edit(setRole)(event.target.value)} className={field} />
      <label className={label} htmlFor="job-topics">Important topics (comma separated)</label>
      <input id="job-topics" value={topics} onChange={(event) => edit(setTopics)(event.target.value)} className={field} />
      <label className={label} htmlFor="job-description">Job description</label>
      <textarea id="job-description" rows={4} value={description} onChange={(event) => edit(setDescription)(event.target.value)} className={field} />
      <button type="button" onClick={() => void save()} className={button}>
        {saved ? 'Job context saved ✓' : 'Save job context'}
      </button>
    </>
  )
}

function MemorySection({ copilot }: { copilot: CopilotApi }) {
  const text = copilot.memory?.text ?? copilot.status?.memory ?? ''
  const turns = copilot.memory?.absorbedTurns ?? copilot.status?.memoryTurns ?? 0
  return (
    <details className="mt-6">
      <summary className="cursor-pointer text-sm font-semibold text-stone-300">Interview memory ({turns} turns)</summary>
      {copilot.memory?.error && <p className="mt-2 text-xs text-amber-200/80">Last update failed: {copilot.memory.error}</p>}
      <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap rounded-lg bg-black/25 p-3 text-xs leading-5 text-stone-400">
        {text || 'Builds up in the background after a few exchanges.'}
      </pre>
    </details>
  )
}

function TestTurnSection({ copilot, onTestTurn }: { copilot: CopilotApi; onTestTurn: Props['onTestTurn'] }) {
  const [speaker, setSpeaker] = useState<Speaker>('interviewer')
  const [text, setText] = useState('')

  async function send() {
    const trimmed = text.trim()
    if (!trimmed) return
    await onTestTurn(speaker, trimmed)
    setText('')
    setSpeaker((current) => (current === 'interviewer' ? 'candidate' : 'interviewer'))
  }

  return (
    <>
      <h3 className="mt-6 text-sm font-semibold text-stone-300">Test without audio</h3>
      <p className="mt-1 text-xs leading-5 text-stone-500">Typed turns go through the same conversation state and model path as transcribed speech. As “Me”, type “Låt oss se” to test screen assistance.</p>
      <div className="mt-2 flex gap-2">
        <select value={speaker} onChange={(event) => setSpeaker(event.target.value as Speaker)} className="rounded-lg border border-white/10 bg-black/30 px-2 py-2 text-sm">
          <option value="interviewer">Interviewer</option>
          <option value="candidate">Me</option>
        </select>
        <button type="button" onClick={() => void copilot.clearConversation()} className="ml-auto rounded-lg border border-white/10 px-3 py-2 text-xs text-stone-400">
          Clear conversation
        </button>
      </div>
      <textarea
        rows={2}
        value={text}
        placeholder="Kan du berätta om ett tekniskt problem du löste?"
        onChange={(event) => setText(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === 'Enter' && !event.shiftKey) {
            event.preventDefault()
            void send()
          }
        }}
        className={field}
      />
      <button type="button" disabled={!text.trim()} onClick={() => void send()} className="mt-2 w-full rounded-lg bg-sky-300/90 px-4 py-2 text-sm font-semibold text-sky-950 disabled:opacity-40">
        Add simulated turn
      </button>
    </>
  )
}
