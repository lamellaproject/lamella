// Lamella managed corlib (from scratch). -- System.Security.Cryptography.X509Certificates.PemField
#if LAMELLA_SURFACE_NET_TLS && LAMELLA_SURFACE_NET_5_0
namespace System.Security.Cryptography.X509Certificates
{

    /// <summary>One PEM field found in a text: where it starts and ends, its label, and its decoded content.</summary>
    internal sealed class PemField
    {
        private const string BeginPrefix = "-----BEGIN ";
        private const string EndPrefix = "-----END ";
        private const string Dashes = "-----";

        /// <summary>The index of the field's first character, the first '-' of its opening boundary.</summary>
        internal int Start;

        /// <summary>The index just past the field's last character, the last '-' of its closing boundary.</summary>
        internal int End;

        /// <summary>The label both boundaries carry.</summary>
        internal string Label;

        /// <summary>The decoded base64 content.</summary>
        internal byte[] Data;

        /// <summary>The first field of <paramref name="text"/> whose label is one of <paramref name="labels"/>, or null.</summary>
        internal static PemField Find(string text, string[] labels)
        {
            int search = 0;
            while (true)
            {
                int begin = text.IndexOf(BeginPrefix, search);
                if (begin < 0) return null;
                search = begin + BeginPrefix.Length;
                if (begin > 0 && !IsWhiteSpace(text[begin - 1])) continue;
                int labelStart = begin + BeginPrefix.Length;
                int labelEnd = text.IndexOf(Dashes, labelStart);
                if (labelEnd < 0) return null;
                string label = text.Substring(labelStart, labelEnd - labelStart);
                if (!IsLabel(label)) continue;
                int contentStart = labelEnd + Dashes.Length;
                string closing = EndPrefix + label + Dashes;
                int contentEnd = text.IndexOf(closing, contentStart);
                if (contentEnd < 0) continue;
                int fieldEnd = contentEnd + closing.Length;
                if (fieldEnd < text.Length && !IsWhiteSpace(text[fieldEnd])) continue;
                byte[] data = DecodeBase64(text, contentStart, contentEnd);
                if ((object)data == null) continue;
                if (!IsOneOf(label, labels))
                {
                    search = fieldEnd;
                    continue;
                }
                PemField field = new PemField();
                field.Start = begin;
                field.End = fieldEnd;
                field.Label = label;
                field.Data = data;
                return field;
            }
        }

        private static bool IsOneOf(string label, string[] labels)
        {
            for (int i = 0; i < labels.Length; i++)
            {
                if (label == labels[i]) return true;
            }
            return false;
        }

        private static bool IsWhiteSpace(char c)
        {
            return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\v' || c == '\f';
        }

        private static bool IsLabel(string label)
        {
            bool previousWasLabelChar = false;
            for (int i = 0; i < label.Length; i++)
            {
                char c = label[i];
                if (c > ' ' && c <= '~' && c != '-')
                {
                    previousWasLabelChar = true;
                }
                else if ((c == '-' || c == ' ') && previousWasLabelChar)
                {
                    previousWasLabelChar = false;
                }
                else
                {
                    return false;
                }
            }
            return label.Length == 0 || previousWasLabelChar;
        }

        private static byte[] DecodeBase64(string text, int start, int end)
        {
            int digits = 0;
            int padding = 0;
            for (int i = start; i < end; i++)
            {
                char c = text[i];
                if (IsWhiteSpace(c)) continue;
                if (c == '=')
                {
                    padding++;
                    continue;
                }
                if (padding > 0 || Base64Value(c) < 0) return null;
                digits++;
            }
            if (padding > 2 || (digits + padding) % 4 != 0) return null;
            byte[] data = new byte[(digits * 6) / 8];
            int count = 0;
            int accumulator = 0;
            int bits = 0;
            for (int i = start; i < end && count < data.Length; i++)
            {
                int value = Base64Value(text[i]);
                if (value < 0) continue;
                accumulator = (accumulator << 6) | value;
                bits += 6;
                if (bits >= 8)
                {
                    bits -= 8;
                    data[count++] = (byte)((accumulator >> bits) & 0xFF);
                }
            }
            return data;
        }

        private static int Base64Value(char c)
        {
            if (c >= 'A' && c <= 'Z') return c - 'A';
            if (c >= 'a' && c <= 'z') return c - 'a' + 26;
            if (c >= '0' && c <= '9') return c - '0' + 52;
            if (c == '+') return 62;
            if (c == '/') return 63;
            return -1;
        }

        /// <summary>
        /// The algorithm OID (its content bytes) of a certificate's subject public key, or null when the
        /// DER is not a certificate this walk can read.
        /// </summary>
        internal static byte[] CertificateKeyAlgorithm(byte[] der)
        {
            int[] tlv = new int[2];
            if (!Read(der, 0, der.Length, 0x30, tlv) || tlv[0] + tlv[1] != der.Length) return null;
            if (!Read(der, tlv[0], tlv[0] + tlv[1], 0x30, tlv)) return null;
            int at = tlv[0];
            int end = tlv[0] + tlv[1];
            if (at < end && der[at] == 0xA0)
            {
                if (!Read(der, at, end, 0xA0, tlv)) return null;
                at = tlv[0] + tlv[1];
            }
            int[] skipped = { 0x02, 0x30, 0x30, 0x30, 0x30 };
            for (int i = 0; i < skipped.Length; i++)
            {
                if (!Read(der, at, end, skipped[i], tlv)) return null;
                at = tlv[0] + tlv[1];
            }
            if (!Read(der, at, end, 0x30, tlv)) return null;
            return FirstOid(der, tlv[0], tlv[0] + tlv[1], tlv);
        }

        /// <summary>The algorithm OID (its content bytes) of a PKCS#8 private key, or null when the DER is not one.</summary>
        internal static byte[] Pkcs8KeyAlgorithm(byte[] der)
        {
            int[] tlv = new int[2];
            if (!Read(der, 0, der.Length, 0x30, tlv) || tlv[0] + tlv[1] != der.Length) return null;
            int end = tlv[0] + tlv[1];
            if (!Read(der, tlv[0], end, 0x02, tlv)) return null;
            int at = tlv[0] + tlv[1];
            if (!Read(der, at, end, 0x30, tlv)) return null;
            int algorithmEnd = tlv[0] + tlv[1];
            byte[] oid = FirstOid(der, at, algorithmEnd, tlv);
            if ((object)oid == null || !Read(der, algorithmEnd, end, 0x04, tlv)) return null;
            return oid;
        }

        private static byte[] FirstOid(byte[] der, int at, int end, int[] tlv)
        {
            if (!Read(der, at, end, 0x30, tlv)) return null;
            if (!Read(der, tlv[0], tlv[0] + tlv[1], 0x06, tlv) || tlv[1] == 0) return null;
            byte[] oid = new byte[tlv[1]];
            Array.Copy(der, tlv[0], oid, 0, tlv[1]);
            return oid;
        }

        private static bool Read(byte[] der, int at, int end, int tag, int[] tlv)
        {
            if (at < 0 || end > der.Length || at + 2 > end || der[at] != tag) return false;
            int first = der[at + 1];
            int position = at + 2;
            int length;
            if (first < 0x80)
            {
                length = first;
            }
            else
            {
                int count = first & 0x7F;
                if (count == 0 || count > 4 || position + count > end) return false;
                length = 0;
                for (int i = 0; i < count; i++)
                {
                    if (length > 0x7FFFFF) return false;
                    length = (length << 8) | der[position++];
                }
            }
            if (length < 0 || length > end - position) return false;
            tlv[0] = position;
            tlv[1] = length;
            return true;
        }

        /// <summary>An OID's dotted text, from its DER content bytes.</summary>
        internal static string OidText(byte[] oid)
        {
            System.Text.StringBuilder text = new System.Text.StringBuilder();
            long value = 0;
            bool first = true;
            for (int i = 0; i < oid.Length; i++)
            {
                value = (value << 7) | (long)(oid[i] & 0x7F);
                if ((oid[i] & 0x80) != 0) continue;
                if (first)
                {
                    long arc = value < 80 ? value / 40 : 2;
                    text.Append(arc);
                    text.Append('.');
                    text.Append(value - arc * 40);
                    first = false;
                }
                else
                {
                    text.Append('.');
                    text.Append(value);
                }
                value = 0;
            }
            return text.ToString();
        }
    }
}
#endif
