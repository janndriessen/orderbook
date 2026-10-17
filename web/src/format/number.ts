const LOCALE = "en-US";

const formatters = new Map<number, Intl.NumberFormat>();

function formatterFor(fractionDigits: number): Intl.NumberFormat {
  let formatter = formatters.get(fractionDigits);
  if (!formatter) {
    formatter = new Intl.NumberFormat(LOCALE, {
      minimumFractionDigits: fractionDigits,
      maximumFractionDigits: fractionDigits,
    });
    formatters.set(fractionDigits, formatter);
  }
  return formatter;
}

/**
 * Formats an integer count of 10^-`decimals` units, e.g. ticks or lots.
 *
 * The value is turned into an exact decimal string before it reaches `Intl`,
 * so dividing by a power of ten never introduces float error.
 */
export function formatUnits(units: number, decimals: number): string {
  const digits = Math.abs(Math.trunc(units))
    .toString()
    .padStart(decimals + 1, "0");
  const split = digits.length - decimals;
  const decimal =
    decimals === 0
      ? digits
      : `${digits.slice(0, split)}.${digits.slice(split)}`;
  const sign = units < 0 ? "-" : "";
  return formatterFor(decimals).format(`${sign}${decimal}` as `${number}`);
}

const percent = new Intl.NumberFormat(LOCALE, {
  style: "percent",
  minimumFractionDigits: 3,
  maximumFractionDigits: 3,
});

/**
 * Spread as a percentage of the mid price, e.g. "0.001%".
 *
 * Both prices are in ticks, so the tick size cancels out of the ratio. The
 * result is for display only, which is why a float division is fine here.
 */
export function formatSpreadPercent(
  bidTicks: number,
  askTicks: number,
): string {
  return percent.format(((askTicks - bidTicks) * 2) / (bidTicks + askTicks));
}

const clock = new Intl.DateTimeFormat(LOCALE, {
  hourCycle: "h23",
  hour: "2-digit",
  minute: "2-digit",
  second: "2-digit",
  fractionalSecondDigits: 3,
  timeZone: "UTC",
});

/** Venue time (microseconds since the epoch) as HH:MM:SS.mmm in UTC. */
export function formatClock(micros: number): string {
  return clock.format(Math.floor(micros / 1000));
}

/** Venue time as an ISO 8601 timestamp, for machine-readable markup. */
export function isoTime(micros: number): string {
  return new Date(Math.floor(micros / 1000)).toISOString();
}
