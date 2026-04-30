import { useEffect, useMemo, useState } from "react";
import { Card } from "../../components/primitives";
import {
  audioSessionsList,
  type AudioSessionInfo,
  type HostConfig,
} from "../../api";

interface Props {
  cfg: HostConfig;
  updateCfg: (next: HostConfig) => void;
}

/**
 * Audio-source toggle list. Enumerates audio sessions on the
 * default render endpoint via `audio_sessions_list`, joins them
 * against the host config's `audio.blacklist`, and toggles entries
 * in/out of the blacklist on click.
 *
 * Changes apply on the next stream start — the running audio loop
 * captures based on the blacklist at the moment the host pipeline
 * was launched.
 */
export default function AudioSourcesCard({ cfg, updateCfg }: Props) {
  const [audioSessions, setAudioSessions] = useState<AudioSessionInfo[]>([]);
  const [audioSessionsError, setAudioSessionsError] = useState<string | null>(
    null,
  );

  const refreshAudioSessions = async () => {
    try {
      setAudioSessions(await audioSessionsList());
      setAudioSessionsError(null);
    } catch (e) {
      console.error("audio_sessions_list failed", e);
      setAudioSessionsError(String(e));
    }
  };

  useEffect(() => {
    void refreshAudioSessions();
  }, []);

  // Blacklist matches process names case-insensitively, mirroring
  // the host crate's loop. Use a Set for O(1) lookups in render.
  const blacklist = useMemo(() => {
    const set = new Set<string>();
    for (const name of cfg.audio.blacklist) set.add(name.toLowerCase());
    return set;
  }, [cfg.audio.blacklist]);

  const isEnabled = (session: AudioSessionInfo) =>
    !blacklist.has(session.processName.toLowerCase());

  const toggleAudioSource = (session: AudioSessionInfo) => {
    const lower = session.processName.toLowerCase();
    const enabled = !blacklist.has(lower);
    // Currently enabled → add to blacklist; currently blacklisted
    // → remove.
    const next = enabled
      ? [...cfg.audio.blacklist, session.processName]
      : cfg.audio.blacklist.filter((n) => n.toLowerCase() !== lower);
    updateCfg({ ...cfg, audio: { ...cfg.audio, blacklist: next } });
  };

  // Coalesce duplicate process entries (Chrome / Discord spawn one
  // session per renderer / voice channel — render the most-active
  // row per process).
  const audioRows = useMemo(() => {
    const stateRank: Record<string, number> = {
      Active: 0,
      Inactive: 1,
      Expired: 2,
      Unknown: 3,
    };
    const map = new Map<string, AudioSessionInfo>();
    for (const s of audioSessions) {
      const key = s.processName.toLowerCase();
      const existing = map.get(key);
      if (
        !existing ||
        (stateRank[s.state] ?? 9) < (stateRank[existing.state] ?? 9)
      ) {
        map.set(key, s);
      }
    }
    return Array.from(map.values()).sort((a, b) => {
      // System sounds last; otherwise active first, then alpha.
      if (a.isSystem !== b.isSystem) return a.isSystem ? 1 : -1;
      const sr = (stateRank[a.state] ?? 9) - (stateRank[b.state] ?? 9);
      if (sr !== 0) return sr;
      return a.processName.localeCompare(b.processName);
    });
  }, [audioSessions]);

  return (
    <Card>
      <div className="cardhd">
        <span className="cardhd__t">Audio sources</span>
        <div className="cardhd__r">
          <span
            className="mono"
            style={{ fontSize: 11, color: "var(--fg-3)" }}
          >
            {audioRows.length === 0
              ? "no sessions"
              : `${audioRows.length} ${audioRows.length === 1 ? "source" : "sources"}`}
          </span>
          <button
            className="share__copy"
            onClick={() => {
              void refreshAudioSessions();
            }}
            title="Re-enumerate audio sessions"
          >
            Refresh
          </button>
        </div>
      </div>
      {audioSessionsError ? (
        <div className="peer__empty">
          Couldn't enumerate audio sessions
          <span className="mono">{audioSessionsError}</span>
        </div>
      ) : audioRows.length === 0 ? (
        <div className="peer__empty">
          No audio sessions on the default render endpoint
          <span className="mono">
            Click Refresh once an app starts producing audio
          </span>
        </div>
      ) : (
        <div className="audio-list">
          {audioRows.map((session) => {
            const enabled = isEnabled(session);
            const label =
              session.displayName && !session.isSystem
                ? session.displayName
                : session.processName;
            return (
              <div
                key={session.pid + session.processName}
                className="audio-row"
              >
                <div className="audio-row__main">
                  <div className="audio-row__name" title={label}>
                    {label}
                  </div>
                  <div className="audio-row__sub">
                    <span className="mono">{session.processName}</span>
                    {" · "}
                    {session.state.toLowerCase()}
                    {session.isSystem ? " · system sounds" : ""}
                  </div>
                </div>
                <div
                  className={`switch ${enabled ? "on" : ""}`}
                  onClick={() => toggleAudioSource(session)}
                  title={
                    enabled
                      ? "Click to exclude this app's audio from the stream"
                      : "Click to include this app's audio in the stream"
                  }
                />
              </div>
            );
          })}
        </div>
      )}
      <div className="share__hint">
        Toggle each source on to include its audio in the streamed
        mix. Changes take effect on the next stream start.
      </div>
    </Card>
  );
}
