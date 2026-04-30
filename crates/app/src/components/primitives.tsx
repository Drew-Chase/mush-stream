import type {
  ButtonHTMLAttributes,
  ComponentType,
  ReactNode,
} from "react";

export type TagKind = "def" | "live" | "host" | "warn";

export const Tag = ({
  kind,
  children,
}: {
  kind?: TagKind;
  children: ReactNode;
}) => <span className={`tag tag--${kind ?? "def"}`}>{children}</span>;

export const Card = ({
  className = "",
  children,
}: {
  className?: string;
  children: ReactNode;
}) => <div className={`card ${className}`}>{children}</div>;

type IconCmp = ComponentType<{ size?: number }>;

export const Btn = ({
  kind = "",
  icon: Icon,
  children,
  ...rest
}: {
  kind?: "" | "primary" | "host" | "live" | "danger" | "ghost";
  icon?: IconCmp;
  children?: ReactNode;
} & ButtonHTMLAttributes<HTMLButtonElement>) => (
  <button className={`btn btn--${kind}`} {...rest}>
    {Icon && <Icon size={15} />}
    {children}
  </button>
);

export const Field = ({
  label,
  mono,
  suffix,
  children,
}: {
  label: string;
  mono?: boolean;
  suffix?: ReactNode;
  children: ReactNode;
}) => (
  <label className={`field ${mono ? "field--mono" : ""}`}>
    <span className="field__lbl">{label}</span>
    <span className="field__c">
      {children}
      {suffix && <span className="field__suf">{suffix}</span>}
    </span>
  </label>
);

export const Stat = ({
  label,
  value,
  unit,
  accent,
  hint,
}: {
  label: string;
  value: ReactNode;
  unit?: string;
  accent?: "live" | "host";
  hint?: ReactNode;
}) => (
  <div>
    <div className="stat__lbl">{label}</div>
    <div className="stat__row">
      <span className={`stat__v ${accent ? "stat__v--" + accent : ""}`}>
        {value}
      </span>
      {unit && <span className="stat__u">{unit}</span>}
    </div>
    {hint && <div className="stat__hint">{hint}</div>}
  </div>
);

export function Sparkline({
  data,
  color = "var(--live)",
}: {
  data: number[];
  color?: string;
}) {
  if (!data || !data.length) return <svg className="spark" />;
  const w = 100;
  const h = 100;
  const min = Math.min(...data);
  const max = Math.max(...data);
  const range = max - min || 1;
  const pts = data.map(
    (v, i) =>
      [
        (i / (data.length - 1)) * w,
        h - ((v - min) / range) * h,
      ] as const,
  );
  const line = pts
    .map((p, i) => `${i ? "L" : "M"}${p[0].toFixed(1)} ${p[1].toFixed(1)}`)
    .join(" ");
  const area = `${line} L${w} ${h} L0 ${h} Z`;
  return (
    <svg
      className="spark"
      viewBox={`0 0 ${w} ${h}`}
      preserveAspectRatio="none"
      style={{ color }}
    >
      <path className="area" d={area} />
      <path d={line} stroke={color} />
    </svg>
  );
}
