using System;
using System.Collections;
using System.Collections.Generic;
using System.Globalization;
using System.Text;

namespace Aurix.Protocol
{
    /// <summary>
    /// Dependency-free JSON reader/writer (objects → <c>Dictionary&lt;string, object&gt;</c>, arrays → <c>List&lt;object&gt;</c>,
    /// numbers → <c>double</c>, plus string/bool/null). Unity's JsonUtility cannot express the server's
    /// <c>{"type": ..., "data": ...}</c> tagged unions, so the SDK ships its own small parser.
    /// </summary>
    public static class MiniJson
    {
        public static object Parse(string json)
        {
            if (json == null) throw new ArgumentNullException(nameof(json));
            int pos = 0;
            var v = ParseValue(json, ref pos);
            SkipWs(json, ref pos);
            if (pos != json.Length) throw new FormatException("trailing characters in JSON");
            return v;
        }

        public static string Serialize(object value)
        {
            var sb = new StringBuilder();
            Write(sb, value);
            return sb.ToString();
        }

        // ---- helpers for typed access -------------------------------------------------------

        public static Dictionary<string, object> AsObject(object v) => v as Dictionary<string, object>;
        public static List<object> AsArray(object v) => v as List<object>;

        public static string GetString(Dictionary<string, object> o, string key) =>
            o != null && o.TryGetValue(key, out var v) ? v as string : null;

        public static bool GetBool(Dictionary<string, object> o, string key, bool fallback = false) =>
            o != null && o.TryGetValue(key, out var v) && v is bool b ? b : fallback;

        public static double GetNumber(Dictionary<string, object> o, string key, double fallback = 0) =>
            o != null && o.TryGetValue(key, out var v) && v is double d ? d : fallback;

        public static uint GetUInt32(Dictionary<string, object> o, string key) => (uint)GetNumber(o, key);

        public static Guid? GetGuid(Dictionary<string, object> o, string key) =>
            Guid.TryParse(GetString(o, key), out var g) ? g : (Guid?)null;

        // ---- writer ---------------------------------------------------------------------------

        private static void Write(StringBuilder sb, object v)
        {
            switch (v)
            {
                case null: sb.Append("null"); break;
                case string s: WriteString(sb, s); break;
                case bool b: sb.Append(b ? "true" : "false"); break;
                case Guid g: WriteString(sb, g.ToString("D")); break;
                case IDictionary<string, object> o:
                    sb.Append('{');
                    bool first = true;
                    foreach (var kv in o)
                    {
                        if (!first) sb.Append(',');
                        first = false;
                        WriteString(sb, kv.Key);
                        sb.Append(':');
                        Write(sb, kv.Value);
                    }
                    sb.Append('}');
                    break;
                case IEnumerable e when !(v is string):
                    sb.Append('[');
                    bool f2 = true;
                    foreach (var item in e)
                    {
                        if (!f2) sb.Append(',');
                        f2 = false;
                        Write(sb, item);
                    }
                    sb.Append(']');
                    break;
                case float f: sb.Append(f.ToString("R", CultureInfo.InvariantCulture)); break;
                case double d: sb.Append(d.ToString("R", CultureInfo.InvariantCulture)); break;
                case IFormattable n: sb.Append(n.ToString(null, CultureInfo.InvariantCulture)); break;
                default: WriteString(sb, v.ToString()); break;
            }
        }

        private static void WriteString(StringBuilder sb, string s)
        {
            sb.Append('"');
            foreach (var c in s)
            {
                switch (c)
                {
                    case '"': sb.Append("\\\""); break;
                    case '\\': sb.Append("\\\\"); break;
                    case '\n': sb.Append("\\n"); break;
                    case '\r': sb.Append("\\r"); break;
                    case '\t': sb.Append("\\t"); break;
                    case '\b': sb.Append("\\b"); break;
                    case '\f': sb.Append("\\f"); break;
                    default:
                        if (c < 0x20) sb.Append("\\u").Append(((int)c).ToString("x4"));
                        else sb.Append(c);
                        break;
                }
            }
            sb.Append('"');
        }

        // ---- reader ---------------------------------------------------------------------------

        private static void SkipWs(string s, ref int p)
        {
            while (p < s.Length && (s[p] == ' ' || s[p] == '\t' || s[p] == '\n' || s[p] == '\r')) p++;
        }

        private static object ParseValue(string s, ref int p)
        {
            SkipWs(s, ref p);
            if (p >= s.Length) throw new FormatException("unexpected end of JSON");
            char c = s[p];
            switch (c)
            {
                case '{': return ParseObject(s, ref p);
                case '[': return ParseArray(s, ref p);
                case '"': return ParseString(s, ref p);
                case 't': Expect(s, ref p, "true"); return true;
                case 'f': Expect(s, ref p, "false"); return false;
                case 'n': Expect(s, ref p, "null"); return null;
                default: return ParseNumber(s, ref p);
            }
        }

        private static void Expect(string s, ref int p, string lit)
        {
            if (string.CompareOrdinal(s, p, lit, 0, lit.Length) != 0) throw new FormatException("invalid literal");
            p += lit.Length;
        }

        private static Dictionary<string, object> ParseObject(string s, ref int p)
        {
            var o = new Dictionary<string, object>();
            p++; // {
            SkipWs(s, ref p);
            if (p < s.Length && s[p] == '}') { p++; return o; }
            while (true)
            {
                SkipWs(s, ref p);
                if (p >= s.Length || s[p] != '"') throw new FormatException("expected object key");
                var key = ParseString(s, ref p);
                SkipWs(s, ref p);
                if (p >= s.Length || s[p] != ':') throw new FormatException("expected ':'");
                p++;
                o[key] = ParseValue(s, ref p);
                SkipWs(s, ref p);
                if (p >= s.Length) throw new FormatException("unterminated object");
                if (s[p] == ',') { p++; continue; }
                if (s[p] == '}') { p++; return o; }
                throw new FormatException("expected ',' or '}'");
            }
        }

        private static List<object> ParseArray(string s, ref int p)
        {
            var a = new List<object>();
            p++; // [
            SkipWs(s, ref p);
            if (p < s.Length && s[p] == ']') { p++; return a; }
            while (true)
            {
                a.Add(ParseValue(s, ref p));
                SkipWs(s, ref p);
                if (p >= s.Length) throw new FormatException("unterminated array");
                if (s[p] == ',') { p++; continue; }
                if (s[p] == ']') { p++; return a; }
                throw new FormatException("expected ',' or ']'");
            }
        }

        private static string ParseString(string s, ref int p)
        {
            p++; // opening quote
            var sb = new StringBuilder();
            while (p < s.Length)
            {
                char c = s[p++];
                if (c == '"') return sb.ToString();
                if (c != '\\') { sb.Append(c); continue; }
                if (p >= s.Length) break;
                char e = s[p++];
                switch (e)
                {
                    case '"': sb.Append('"'); break;
                    case '\\': sb.Append('\\'); break;
                    case '/': sb.Append('/'); break;
                    case 'b': sb.Append('\b'); break;
                    case 'f': sb.Append('\f'); break;
                    case 'n': sb.Append('\n'); break;
                    case 'r': sb.Append('\r'); break;
                    case 't': sb.Append('\t'); break;
                    case 'u':
                        if (p + 4 > s.Length) throw new FormatException("bad unicode escape");
                        sb.Append((char)int.Parse(s.Substring(p, 4), NumberStyles.HexNumber, CultureInfo.InvariantCulture));
                        p += 4;
                        break;
                    default: throw new FormatException("bad escape");
                }
            }
            throw new FormatException("unterminated string");
        }

        private static object ParseNumber(string s, ref int p)
        {
            int start = p;
            if (p < s.Length && (s[p] == '-' || s[p] == '+')) p++;
            while (p < s.Length && (char.IsDigit(s[p]) || s[p] == '.' || s[p] == 'e' || s[p] == 'E' || s[p] == '-' || s[p] == '+')) p++;
            var text = s.Substring(start, p - start);
            if (!double.TryParse(text, NumberStyles.Float, CultureInfo.InvariantCulture, out var d))
                throw new FormatException("invalid number: " + text);
            return d;
        }
    }
}
