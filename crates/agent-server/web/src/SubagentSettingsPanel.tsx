import { useEffect, useMemo, useRef, useState } from 'react'
import {
  ArrowLeft,
  Bot,
  ChevronRight,
  CircleAlert,
  ImagePlus,
  Plus,
  RotateCcw,
  Save,
  Search,
  Trash2,
  Upload,
  X,
} from 'lucide-react'
import { fetchJson } from './api'
import type {
  SubagentProfileResponse,
  SubagentProfileWriteRequest,
  SubagentSettingsResponse,
} from './types'

const maxSourceAvatarBytes = 5 * 1024 * 1024
const avatarSize = 256
const acceptedAvatarTypes = ['image/png', 'image/jpeg', 'image/webp']

type SubagentDraft = {
  id: string | null
  name: string
  avatarDataUrl?: string
}

export default function SubagentSettingsPanel({
  settings,
  onChanged,
}: {
  settings: SubagentSettingsResponse | null
  onChanged: () => Promise<void>
}) {
  const [query, setQuery] = useState('')
  const [draft, setDraft] = useState<SubagentDraft | null>(null)
  const [saving, setSaving] = useState(false)
  const [processingAvatar, setProcessingAvatar] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const fileInputRef = useRef<HTMLInputElement | null>(null)

  const profiles = useMemo(() => {
    const normalized = query.trim().toLocaleLowerCase()
    if (!normalized) return settings?.profiles ?? []
    return (settings?.profiles ?? []).filter((profile) =>
      profile.name.toLocaleLowerCase().includes(normalized),
    )
  }, [query, settings])

  const editProfile = (profile: SubagentProfileResponse) => {
    setDraft({
      id: profile.id,
      name: profile.name,
      avatarDataUrl: profile.avatar_data_url ?? undefined,
    })
    setError(null)
  }

  const createProfile = () => {
    setDraft({ id: null, name: '' })
    setError(null)
  }

  const saveProfile = async () => {
    if (!draft) return
    const validation = validateDraft(draft, settings)
    if (validation) {
      setError(validation)
      return
    }
    setSaving(true)
    setError(null)
    try {
      const request: SubagentProfileWriteRequest = {
        name: draft.name.trim(),
        avatar_data_url: draft.avatarDataUrl ?? null,
      }
      await fetchJson<SubagentProfileResponse>(
        draft.id ? `/api/subagents/${encodeURIComponent(draft.id)}` : '/api/subagents',
        {
          method: draft.id ? 'PUT' : 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify(request),
        },
      )
      await onChanged()
      setDraft(null)
    } catch (caught) {
      setError(errorMessage(caught))
    } finally {
      setSaving(false)
    }
  }

  const deleteProfile = async () => {
    if (!draft?.id) return
    if (!window.confirm(`删除子智能体“${draft.name}”？历史记录将回退为机器人图标。`)) {
      return
    }
    setSaving(true)
    setError(null)
    try {
      await fetchJson<unknown>(`/api/subagents/${encodeURIComponent(draft.id)}`, {
        method: 'DELETE',
      })
      await onChanged()
      setDraft(null)
    } catch (caught) {
      setError(errorMessage(caught))
    } finally {
      setSaving(false)
    }
  }

  const resetProfiles = async () => {
    if (!window.confirm('恢复 22 人默认名单？现有自定义姓名和头像将被全部替换。')) {
      return
    }
    setSaving(true)
    setError(null)
    try {
      await fetchJson<SubagentSettingsResponse>('/api/subagent-settings/reset', {
        method: 'POST',
      })
      await onChanged()
      setDraft(null)
    } catch (caught) {
      setError(errorMessage(caught))
    } finally {
      setSaving(false)
    }
  }

  const chooseAvatar = async (file: File | undefined) => {
    if (!draft || !file) return
    setProcessingAvatar(true)
    setError(null)
    try {
      const avatarDataUrl = await normalizeSubagentAvatar(
        file,
        settings?.max_avatar_bytes ?? 256 * 1024,
      )
      setDraft((current) => current ? { ...current, avatarDataUrl } : current)
    } catch (caught) {
      setError(errorMessage(caught))
    } finally {
      setProcessingAvatar(false)
      if (fileInputRef.current) fileInputRef.current.value = ''
    }
  }

  return (
    <section className="settings-page resource-settings-page subagent-settings-page" aria-labelledby="subagent-settings-title">
      <header className="settings-page-header resource-settings-header subagent-settings-header">
        <div>
          <p className="eyebrow">Settings</p>
          <h1 id="subagent-settings-title">子智能体</h1>
          <p>管理每个 Turn 随机使用的姓名与头像。名单变更从下一个 Turn 生效。</p>
        </div>
        {!draft ? (
          <div className="resource-header-actions">
            <button className="secondary-button" type="button" disabled={saving} onClick={() => void resetProfiles()}>
              <RotateCcw size={15} /> 恢复默认名单
            </button>
            <button
              className="approve-button"
              type="button"
              disabled={saving || (settings?.profiles.length ?? 0) >= (settings?.max_profiles ?? 64)}
              onClick={createProfile}
            >
              <Plus size={16} /> 新建子智能体
            </button>
          </div>
        ) : null}
      </header>

      {draft ? (
        <div className="resource-editor-view">
          <button className="resource-back-link" type="button" onClick={() => setDraft(null)}>
            <ArrowLeft size={16} /> 返回子智能体列表
          </button>
          <form className="resource-form-card subagent-profile-form" onSubmit={(event) => { event.preventDefault(); void saveProfile() }}>
            <div className="resource-form-heading">
              <div>
                <p className="eyebrow">Subagent profile</p>
                <h2>{draft.id ? `编辑 ${draft.name}` : '新建子智能体'}</h2>
                <p>头像仅用于界面展示，不会写入提示词，也不会传给模型。</p>
              </div>
              <span className="scope-badge">全局</span>
            </div>

            <div className="subagent-avatar-editor">
              <SubagentAvatar profile={{ id: draft.id ?? 'new', name: draft.name || '新成员', avatar_data_url: draft.avatarDataUrl }} size="large" />
              <div className="subagent-avatar-actions">
                <strong>{draft.avatarDataUrl ? '已配置头像' : '使用默认机器人图标'}</strong>
                <p>PNG、JPEG 或 WebP，原文件不超过 5 MiB。保存前会居中裁切为 256×256。</p>
                <div>
                  <button className="secondary-button" type="button" disabled={processingAvatar || saving} onClick={() => fileInputRef.current?.click()}>
                    {draft.avatarDataUrl ? <Upload size={15} /> : <ImagePlus size={15} />}
                    {processingAvatar ? '处理中…' : draft.avatarDataUrl ? '替换头像' : '上传头像'}
                  </button>
                  {draft.avatarDataUrl ? (
                    <button className="secondary-button" type="button" disabled={processingAvatar || saving} onClick={() => setDraft({ ...draft, avatarDataUrl: undefined })}>
                      <X size={15} /> 移除头像
                    </button>
                  ) : null}
                </div>
                <input
                  ref={fileInputRef}
                  className="sr-only"
                  type="file"
                  accept={acceptedAvatarTypes.join(',')}
                  onChange={(event) => void chooseAvatar(event.target.files?.[0])}
                />
              </div>
            </div>

            <div className="resource-field-grid">
              <label className="resource-field full">
                <span>姓名</span>
                <input
                  value={draft.name}
                  maxLength={40}
                  placeholder="输入 1–40 个字符"
                  onChange={(event) => setDraft({ ...draft, name: event.target.value })}
                />
              </label>
            </div>

            <div className="resource-form-note">
              同一父 Turn 内会从当前名单随机分配且四路并发不重名；正在执行的 Turn 保留启动时快照。
            </div>
            <div className="resource-form-actions split">
              {draft.id ? (
                <button
                  className="danger-button subtle"
                  type="button"
                  disabled={saving || (settings?.profiles.length ?? 0) <= (settings?.min_profiles ?? 4)}
                  onClick={() => void deleteProfile()}
                >
                  <Trash2 size={15} /> 删除子智能体
                </button>
              ) : <span />}
              <button className="approve-button" type="submit" disabled={saving || processingAvatar}>
                <Save size={16} /> {saving ? '保存中…' : '保存子智能体'}
              </button>
            </div>
          </form>
          {error ? <SubagentSettingsError message={error} /> : null}
        </div>
      ) : (
        <div className="resource-list-view">
          <label className="resource-search">
            <Search size={17} />
            <input value={query} placeholder="搜索子智能体…" onChange={(event) => setQuery(event.target.value)} />
            {query ? <button type="button" title="清除搜索" onClick={() => setQuery('')}><X size={15} /></button> : null}
          </label>
          <div className="resource-list-heading">
            <strong>全局名单</strong>
            <span>{profiles.length} / {settings?.profiles.length ?? 0} 项</span>
          </div>
          <div className="resource-list-card subagent-profile-list">
            {!settings ? (
              <div className="resource-empty">
                <Bot size={28} />
                <strong>正在加载子智能体名单…</strong>
              </div>
            ) : profiles.length === 0 ? (
              <div className="resource-empty">
                <Bot size={28} />
                <strong>没有匹配的子智能体</strong>
                <span>清除搜索条件后再试。</span>
              </div>
            ) : null}
            {profiles.map((profile) => (
              <button className="resource-list-row subagent-profile-row" type="button" key={profile.id} onClick={() => editProfile(profile)}>
                <SubagentAvatar profile={profile} />
                <span className="resource-list-copy">
                  <span><strong>{profile.name}</strong><small>{profile.avatar_data_url ? '已配置头像' : '默认图标'}</small></span>
                  <small>{profile.id}</small>
                </span>
                <ChevronRight size={16} aria-hidden="true" />
              </button>
            ))}
          </div>
          {settings ? (
            <div className="resource-form-note subagent-store-note">
              名单限制 {settings.min_profiles}–{settings.max_profiles} 人 · 保存在 {settings.store_path}
            </div>
          ) : null}
          {error ? <SubagentSettingsError message={error} /> : null}
        </div>
      )}
    </section>
  )
}

export function SubagentAvatar({
  profile,
  size = 'normal',
}: {
  profile: SubagentProfileResponse
  size?: 'normal' | 'large'
}) {
  const [imageFailed, setImageFailed] = useState(false)
  useEffect(() => setImageFailed(false), [profile.avatar_data_url])
  const avatar = profile.avatar_data_url || undefined
  const showImage = Boolean(avatar) && !imageFailed
  return (
    <span className={`subagent-profile-avatar ${size}`} aria-label={`${profile.name}头像`}>
      {showImage ? (
        <img src={avatar} alt="" onError={() => setImageFailed(true)} />
      ) : (
        <Bot size={size === 'large' ? 31 : 18} aria-hidden="true" />
      )}
    </span>
  )
}

function SubagentSettingsError({ message }: { message: string }) {
  return <div className="model-settings-error resource-error" role="alert"><CircleAlert size={17} /><span>{message}</span></div>
}

function validateDraft(
  draft: SubagentDraft,
  settings: SubagentSettingsResponse | null,
): string | null {
  const name = draft.name.trim()
  if (!name || [...name].length > 40) return '姓名去除首尾空白后需为 1–40 个字符。'
  const duplicate = settings?.profiles.some(
    (profile) => profile.id !== draft.id && profile.name.trim().toLocaleLowerCase() === name.toLocaleLowerCase(),
  )
  return duplicate ? '姓名不能与名单中的其他子智能体重复。' : null
}

export async function normalizeSubagentAvatar(
  file: File,
  maxResultBytes: number,
): Promise<string> {
  if (!acceptedAvatarTypes.includes(file.type)) {
    throw new Error('头像仅支持 PNG、JPEG 和 WebP；不接受 SVG 或 GIF。')
  }
  if (file.size <= 0 || file.size > maxSourceAvatarBytes) {
    throw new Error('头像原文件必须大于 0 且不超过 5 MiB。')
  }

  const source = await readFileAsDataUrl(file)
  const image = await loadImage(source)
  if (!image.naturalWidth || !image.naturalHeight) {
    throw new Error('无法读取头像尺寸。')
  }
  const canvas = document.createElement('canvas')
  canvas.width = avatarSize
  canvas.height = avatarSize
  const context = canvas.getContext('2d')
  if (!context) throw new Error('当前浏览器无法处理头像。')

  const cropSize = Math.min(image.naturalWidth, image.naturalHeight)
  const sourceX = (image.naturalWidth - cropSize) / 2
  const sourceY = (image.naturalHeight - cropSize) / 2
  context.drawImage(
    image,
    sourceX,
    sourceY,
    cropSize,
    cropSize,
    0,
    0,
    avatarSize,
    avatarSize,
  )

  for (const [mime, qualities] of [
    ['image/webp', [0.9, 0.82, 0.72, 0.6, 0.48]],
    ['image/jpeg', [0.9, 0.8, 0.68, 0.55, 0.42]],
  ] as const) {
    for (const quality of qualities) {
      const result = canvas.toDataURL(mime, quality)
      if (result.startsWith(`data:${mime};base64,`) && decodedDataUrlBytes(result) <= maxResultBytes) {
        return result
      }
    }
  }

  const png = canvas.toDataURL('image/png')
  if (png.startsWith('data:image/png;base64,') && decodedDataUrlBytes(png) <= maxResultBytes) {
    return png
  }
  throw new Error(`处理后的头像仍超过 ${Math.round(maxResultBytes / 1024)} KiB，请选择更简单的图片。`)
}

function readFileAsDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onerror = () => reject(new Error('读取头像文件失败。'))
    reader.onload = () => typeof reader.result === 'string'
      ? resolve(reader.result)
      : reject(new Error('读取头像文件失败。'))
    reader.readAsDataURL(file)
  })
}

function loadImage(source: string): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const image = new Image()
    image.onload = () => resolve(image)
    image.onerror = () => reject(new Error('头像不是可解码的 PNG、JPEG 或 WebP 图片。'))
    image.src = source
  })
}

function decodedDataUrlBytes(value: string): number {
  const encoded = value.slice(value.indexOf(',') + 1)
  const padding = encoded.endsWith('==') ? 2 : encoded.endsWith('=') ? 1 : 0
  return Math.floor(encoded.length * 3 / 4) - padding
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}
