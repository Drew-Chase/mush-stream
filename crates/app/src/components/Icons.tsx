import type { CSSProperties, ReactNode } from "react";

type IcoProps = { size?: number; style?: CSSProperties; className?: string };

export const Ico = ({
  d,
  size = 18,
  style,
  className,
}: IcoProps & { d: ReactNode }) => (
  <svg
    width={size}
    height={size}
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    strokeWidth="1.7"
    strokeLinecap="round"
    strokeLinejoin="round"
    style={style}
    className={className}
  >
    {d}
  </svg>
);

export const IcHome = (p: IcoProps) => (
  <Ico
    {...p}
    d={
      <>
        <path d="M3 11l9-8 9 8" />
        <path d="M5 10v10h14V10" />
      </>
    }
  />
);

export const IcBroadcast = (p: IcoProps) => (
  <Ico
    {...p}
    d={
      <>
        <circle cx="12" cy="12" r="2.5" />
        <path d="M7.5 7.5a6 6 0 000 9" />
        <path d="M16.5 7.5a6 6 0 010 9" />
        <path d="M4 4a11 11 0 000 16" />
        <path d="M20 4a11 11 0 010 16" />
      </>
    }
  />
);

export const IcConnect = (p: IcoProps) => (
  <Ico
    {...p}
    d={
      <>
        <path d="M9 9V5a3 3 0 016 0v4" />
        <rect x="5" y="9" width="14" height="11" rx="2" />
        <path d="M12 14v3" />
      </>
    }
  />
);

export const IcSettings = (p: IcoProps) => (
  <Ico
    {...p}
    d={
      <>
        <circle cx="12" cy="12" r="3" />
        <path d="M19 12a7 7 0 00-.1-1.2l2-1.5-2-3.4-2.3 1a7 7 0 00-2-1.2L14 3h-4l-.6 2.6a7 7 0 00-2 1.2l-2.3-1-2 3.4 2 1.5A7 7 0 005 12c0 .4 0 .8.1 1.2l-2 1.5 2 3.4 2.3-1a7 7 0 002 1.2L10 21h4l.6-2.6a7 7 0 002-1.2l2.3 1 2-3.4-2-1.5c.1-.4.1-.8.1-1.2z" />
      </>
    }
  />
);

export const IcLogs = (p: IcoProps) => (
  <Ico {...p} d={<path d="M4 6h16M4 12h16M4 18h10" />} />
);

export const IcCopy = (p: IcoProps) => (
  <Ico
    {...p}
    d={
      <>
        <rect x="9" y="9" width="11" height="11" rx="2" />
        <path d="M5 15V5a2 2 0 012-2h10" />
      </>
    }
  />
);

export const IcCheck = (p: IcoProps) => (
  <Ico {...p} d={<path d="M5 13l4 4 10-10" />} />
);

export const IcStop = (p: IcoProps) => (
  <Ico {...p} d={<rect x="6" y="6" width="12" height="12" rx="1" />} />
);

export const IcChevron = (p: IcoProps) => (
  <Ico {...p} d={<path d="M9 6l6 6-6 6" />} />
);

export const IcMonitor = (p: IcoProps) => (
  <Ico
    {...p}
    d={
      <>
        <rect x="3" y="4" width="18" height="13" rx="2" />
        <path d="M8 21h8M12 17v4" />
      </>
    }
  />
);

export const IcEye = (p: IcoProps) => (
  <Ico
    {...p}
    d={
      <>
        <path d="M2 12s4-7 10-7 10 7 10 7-4 7-10 7S2 12 2 12z" />
        <circle cx="12" cy="12" r="3" />
      </>
    }
  />
);

export const IcGamepad = (p: IcoProps) => (
  <Ico
    {...p}
    d={
      <>
        <path d="M6 12h4M8 10v4M14 11h.01M17 13h.01" />
        <path d="M3 17a4 4 0 014-4h10a4 4 0 110 8H7a4 4 0 01-4-4z" />
      </>
    }
  />
);

export const IcZap = (p: IcoProps) => (
  <Ico {...p} d={<path d="M13 2L4 14h7l-1 8 9-12h-7z" />} />
);

export const IcMin = (p: IcoProps) => (
  <Ico {...p} d={<path d="M5 12h14" />} />
);

export const IcMax = (p: IcoProps) => (
  <Ico {...p} d={<rect x="5" y="5" width="14" height="14" />} />
);

export const IcX = (p: IcoProps) => (
  <Ico {...p} d={<path d="M6 6l12 12M18 6L6 18" />} />
);

export const IcSpark = (p: IcoProps) => (
  <Ico {...p} d={<path d="M3 17l5-5 4 4 8-8" />} />
);

export const IcLogo = ({ size = 16 }: { size?: number }) => (
  <svg width={size} height={size} viewBox="0 0 24 24" fill="none">
    <path
      d="M4 12c0-4 3-7 8-7s8 3 8 7v6H4v-6z"
      stroke="currentColor"
      strokeWidth="2"
    />
    <circle cx="9" cy="13" r="1.5" fill="currentColor" />
    <circle cx="15" cy="13" r="1.5" fill="currentColor" />
  </svg>
);
