/**
 * telemetry — 本机统计(R9)。
 *
 * 一个刻意的 tiny counter:`track(event)` 只往 localStorage
 * `swarmx:stats:v1` 里 +1 —— total 累计 + 按天 bucket("YYYY-MM-DD",
 * 本地日期,不含更细时间戳)。**没有网络、没有标识符、没有内容**:
 * 只存在本机,从不上传。设置→隐私 可以查看计数 / 清零 / 关闭收集
 * (关闭后 track 是 no-op,但已存数据保留,由「清零」显式删)。
 *
 * 为什么不用后端:swarmx 的定位是本机工具,任何外发都得先过用户这关;
 * 计数器只回答「我自己到底用没用这个功能」,localStorage 足够。
 */

const STORAGE_KEY = "swarmx:stats:v1";

/** 天桶只保留最近这么多天 —— localStorage 是有配额的,统计不该无界生长。 */
const KEEP_DAYS = 90;

export interface StatsBlob {
  /** 关闭收集开关:true 时 track() 直接返回(已存数据不动)。 */
  disabled: boolean;
  /** 事件名 → 累计次数。 */
  total: Record<string, number>;
  /** "YYYY-MM-DD"(本地日期)→ (事件名 → 当天次数)。 */
  days: Record<string, Record<string, number>>;
}

const EMPTY: StatsBlob = { disabled: false, total: {}, days: {} };

function read(): StatsBlob {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (!raw) return { ...EMPTY, total: {}, days: {} };
    const parsed = JSON.parse(raw) as Partial<StatsBlob> | null;
    if (!parsed || typeof parsed !== "object") return { ...EMPTY, total: {}, days: {} };
    return {
      disabled: parsed.disabled === true,
      total:
        parsed.total && typeof parsed.total === "object" ? parsed.total : {},
      days: parsed.days && typeof parsed.days === "object" ? parsed.days : {},
    };
  } catch {
    return { ...EMPTY, total: {}, days: {} };
  }
}

function write(s: StatsBlob): void {
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(s));
  } catch {
    /* quota 满了就丢这一次 —— 统计永远不能弄坏产品功能 */
  }
}

/** 本地日期(非 UTC)的天桶 key —— 这桶是给本机用户看的,跟他的日历对齐。 */
function dayKey(d: Date): string {
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${d.getFullYear()}-${m}-${day}`;
}

/** +1 一次事件。关闭收集 / 存储不可用时静默 no-op。 */
export function track(event: string): void {
  try {
    const s = read();
    if (s.disabled) return;
    s.total[event] = (s.total[event] ?? 0) + 1;
    const dk = dayKey(new Date());
    const bucket = (s.days[dk] ??= {});
    bucket[event] = (bucket[event] ?? 0) + 1;
    // 修剪 oldest 天桶,把存储占用钉死在 ~KEEP_DAYS 天。
    const keys = Object.keys(s.days).sort();
    while (keys.length > KEEP_DAYS) {
      const oldest = keys.shift();
      if (oldest) delete s.days[oldest];
    }
    write(s);
  } catch {
    /* never break the product over a counter */
  }
}

/** 设置→隐私 的读出用。返回的是解析后的快照,改它不会影响存储。 */
export function readStats(): StatsBlob {
  return read();
}

/** 清零计数,但保留「关闭收集」开关本身 —— 清零不该顺手把收集重新打开。 */
export function clearStats(): void {
  const s = read();
  write({ disabled: s.disabled, total: {}, days: {} });
}

/** 关闭/开启收集。持久化在同一个 stats key 里(设置页是它的唯一 UI)。 */
export function setTelemetryDisabled(disabled: boolean): void {
  const s = read();
  s.disabled = disabled;
  write(s);
}
