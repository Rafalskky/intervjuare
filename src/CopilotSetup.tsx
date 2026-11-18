import { useState } from 'react'
import type { InterviewLanguage, JobContext, Speaker } from './copilotTypes'
import { parseTopics } from './suggestionView'
import type { CopilotApi } from './useCopilot'

type Props = {
  copilot: CopilotApi
  language: InterviewLanguage
  onTestTurn: (speaker: Speaker, text: string) => Promise<void>
}

const field = 'mt-1 w-full rounded-lg border border-white/10 bg-black/30 px-3 py-2 text-sm text-stone-200'
const label = 'mt-4 block text-xs uppercase tracking-wider text-stone-500'

export function CopilotSetup({ copilot, language, onTestTurn }: Props) {
  const { status, hermes } = copilot
  return (
    <div className="mt-5 rounded-2xl border border-white/10 bg-[#11151b] p-5">
      <h2 className="text-lg font-medium">Interview copilot</h2>
      {copilot.error && <p role="alert" className="mt-3 rounded-lg bg-red-400/10 p-3 text-xs leading-5 text-red-100">{copilot.error}</p>}

      <h3 className="mt-5 text-sm font-semibold text-stone-300">Hermes</h3>
      <p className={`mt-2 rounded-lg p-3 text-xs leading-5 ${hermes?.authorized ? 'bg-emerald-400/10 text-emerald-100' : 'bg-amber-300/10 text-amber-100'}`}>
        {copilot.checkingHermes ? 'Checking Hermes…' : (hermes?.message ?? 'Not checked yet.')}
        {hermes && <span className="block text-stone-400">{hermes.config.baseUrl} · key: {hermes.config.keySource}</span>}
      </p>
      <button type="button" disabled={copilot.checkingHermes} onClick={() => void copilot.checkHermes()} className="mt-2 w-full rounded-lg border border-white/10 px-4 py-2 text-sm text-stone-300 disabled:opacity-40">
        Check Hermes connection
      </button>
      <label className="mt-3 flex items-center gap-2 text-sm text-stone-300">
        <input type="checkbox" checked={status?.autoAnswer ?? true} disabled={!status} onChange={(event) => void copilot.setAutoAnswer(event.target.checked)} />
        Answer automatically when the interviewer finishes
      </label>

      {status ? (
        <>
          <ProfileSection copilot={copilot} initialPath={status.profilePath} />
          <JobSection copilot={copilot} language={language} initial={status.job} />
        </>
      ) : (
        <p className="mt-5 text-xs text-stone-500">Loading copilot state…</p>
      )}
      <TestTurnSection copilot={copilot} onTestTurn={onTestTurn} />
    </div>
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
      <button type="button" onClick={() => void copilot.reloadProfile(profilePath)} className="mt-2 w-full rounded-lg border border-white/10 px-4 py-2 text-sm text-stone-300">
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
      <h3 className="mt-6 text-sm font-semibold text-stone-300">Job</h3>
      <label className={label} htmlFor="job-company">Company</label>
      <input id="job-company" value={company} onChange={(event) => edit(setCompany)(event.target.value)} className={field} />
      <label className={label} htmlFor="job-role">Role</label>
      <input id="job-role" value={role} onChange={(event) => edit(setRole)(event.target.value)} className={field} />
      <label className={label} htmlFor="job-topics">Important topics (comma separated)</label>
      <input id="job-topics" value={topics} onChange={(event) => edit(setTopics)(event.target.value)} className={field} />
      <label className={label} htmlFor="job-description">Job description</label>
      <textarea id="job-description" rows={4} value={description} onChange={(event) => edit(setDescription)(event.target.value)} className={field} />
      <button type="button" onClick={() => void save()} className="mt-2 w-full rounded-lg border border-white/10 px-4 py-2 text-sm text-stone-300">
        {saved ? 'Job context applied ✓' : 'Apply job context'}
      </button>
    </>
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
      <p className="mt-1 text-xs leading-5 text-stone-500">Typed turns go through the same conversation state and Hermes path as transcribed speech.</p>
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
