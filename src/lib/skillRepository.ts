import { parseRepoUrl as parseHostedRepo } from "@/components/skills/repoUrl";

export interface SkillRepositoryRef {
  owner: string;
  name: string;
  repoUrl?: string;
  host?: string;
}

export const skillRepositoryUrl = (repo: SkillRepositoryRef) =>
  repo.repoUrl ??
  `https://${repo.host || "github.com"}/${repo.owner}/${repo.name}`;

/** Missing source means GitHub, preserving existing keys and persisted caches. */
export const skillRepositoryKey = (repo: SkillRepositoryRef) =>
  repo.repoUrl ??
  `${repo.host && repo.host !== "github.com" ? `${repo.host}/` : ""}${repo.owner}/${repo.name}`.toLowerCase();

export const skillRepositoryLabel = (repo: SkillRepositoryRef) =>
  repo.repoUrl?.replace(/^https:\/\//, "") ??
  `${repo.host && repo.host !== "github.com" ? `${repo.host}/` : ""}${repo.owner}/${repo.name}`;

/** Only repository roots: never accept embedded credentials, refs or query URLs. */
export function parseSkillRepositoryUrl(
  value: string,
  provider: "auto" | "gitlab" | "forgejo" = "auto",
): SkillRepositoryRef | null {
  const trimmed = value.trim();
  // Validate before URL normalization can erase dot segments or credentials.
  if (
    /[\\%\x00-\x20\x7f]/.test(trimmed) ||
    trimmed.split("/").some((part) => part === "." || part === "..")
  )
    return null;
  if (/^[a-z]+:\/\//i.test(trimmed)) {
    try {
      const url = new URL(trimmed);
      const ssh = url.protocol === "ssh:";
      if (
        (!ssh && url.protocol !== "https:") ||
        url.password ||
        url.search ||
        url.hash ||
        (url.username && !(ssh && url.username === "git"))
      )
        return null;
    } catch {
      return null;
    }
  }
  if (
    provider === "gitlab" ||
    (provider === "auto" &&
      (trimmed.includes("gitlab") ||
        /\/src\/(?:branch|tag|commit)\//.test(trimmed) ||
        /\/-\//.test(trimmed) ||
        /^(?:git@|ssh:\/\/)/i.test(trimmed)))
  ) {
    return parseHostedRepo(trimmed);
  }
  let raw = value
    .trim()
    .replace(/\/+$/, "")
    .replace(/\.git$/, "");
  if (!raw || /[\\%\x00-\x20\x7f]/.test(raw)) return null;
  if (!raw.includes("://")) raw = `https://github.com/${raw}`;
  if (raw.split("/").some((part) => part === "." || part === "..")) return null;
  try {
    const url = new URL(raw);
    if (
      url.protocol !== "https:" ||
      url.username ||
      url.password ||
      url.search ||
      url.hash
    )
      return null;
    const parts = url.pathname.slice(1).split("/");
    if (
      parts.length < 2 ||
      parts.some((part) => !/^[A-Za-z0-9_.-]+$/.test(part))
    )
      return null;
    const [owner, name] = parts.slice(-2);
    if (url.hostname === "github.com") {
      if (parts.length !== 2 || url.port || !/^[A-Za-z0-9-]{1,39}$/.test(owner))
        return null;
      return { owner, name };
    }
    return { owner, name, repoUrl: url.toString().replace(/\/$/, "") };
  } catch {
    return null;
  }
}
