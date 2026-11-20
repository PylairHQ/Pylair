/** The Pylair wordmark, shown once at startup in fancy mode. Consoles that
 * can draw it get the box-drawn art; fancy consoles without Unicode (a
 * PowerShell 5.1 conhost on a non-UTF-8 code page) get one ASCII line. */

const ART =
  "  ┌─┐┬ ┬┬  ┌─┐┬┬─┐\n" +
  "  ├─┘└┬┘│  ├─┤│├┬┘   desktop apps for python\n" +
  "  ┴   ┴ ┴─┘┴ ┴┴┴└─   v";

export function wordmark(unicode: boolean, version: string): string {
  return unicode ? `${ART}${version}` : `pylair ${version}`;
}
