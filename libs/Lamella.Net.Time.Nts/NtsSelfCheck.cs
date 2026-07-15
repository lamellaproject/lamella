// Lamella.Net.Time.Nts -- a DETERMINISTIC end-to-end check of the protected-NTP codec + AEAD chain, driving NtsSession as both client and mock server through the real intrinsics.
using System;

namespace Lamella.Net.Time
{
    public sealed class NtsSelfCheck
    {
        public static int RunAll()
        {
            byte[] c2sKeyBytes = Repeat(0x11, 32);
            byte[] s2cKeyBytes = Repeat(0x22, 32);
            int c2s = NtsNative.ImportKey(c2sKeyBytes);
            int s2c = NtsNative.ImportKey(s2cKeyBytes);
            if (c2s < 0 || s2c < 0) return 10;

            byte[][] clientCookies = new byte[4][];
            clientCookies[0] = Repeat(0xA0, 40);
            clientCookies[1] = Repeat(0xA1, 40);
            NtsSession client = new NtsSession(c2s, s2c, clientCookies, 2);

            byte[] header = NtpHeader(0x23);
            byte[] uniqueId = Repeat(0x5A, 32);
            byte[] clientNonce = Repeat(0x01, 16);
            byte[] request = client.BuildRequest(header, uniqueId, clientNonce, 1);
            if ((object)request == null) return 11;
            if (client.CookieCount != 1) return 12;

            NtsSession server = new NtsSession(c2s, s2c, new byte[0][], 0);
            if (!ServerVerifyRequest(c2s, request, uniqueId, clientNonce)) return 13;

            byte[] tampered = new byte[request.Length];
            for (int i = 0; i < request.Length; i++) tampered[i] = request[i];
            tampered[50] ^= 0x01;
            if (ServerVerifyRequest(c2s, tampered, uniqueId, clientNonce)) return 14;

            byte[] responseHeader = NtpHeader(0x24);
            byte[] serverNonce = Repeat(0x02, 16);
            byte[][] freshCookies = new byte[2][];
            freshCookies[0] = Repeat(0xB0, 40);
            freshCookies[1] = Repeat(0xB1, 40);
            byte[] response = server.BuildResponse(responseHeader, uniqueId, serverNonce, freshCookies, 2);
            if ((object)response == null) return 15;

            if (!client.VerifyResponse(response)) return 16;
            if (client.CookieCount != 2) return 17;

            byte[] forgedHeader = NtpHeader(0x24);
            byte[] forged = server.BuildResponse(forgedHeader, Repeat(0x99, 32), serverNonce, freshCookies, 2);
            if (client.VerifyResponse(forged)) return 18;

            byte[] good = server.BuildResponse(NtpHeader(0x24), uniqueId, serverNonce, freshCookies, 2);
            good[good.Length - 1] ^= 0x01;
            if (client.VerifyResponse(good)) return 19;

            return 42;
        }

        private static bool ServerVerifyRequest(int c2sKey, byte[] request, byte[] expectedUniqueId, byte[] nonce)
        {
            int authOffset = -1;
            byte[] gotUniqueId = null;
            int offset = 48;
            while (offset + 4 <= request.Length)
            {
                int type = NtsProtocol.ReadFieldType(request, offset);
                int len = NtsProtocol.ReadFieldLength(request, offset);
                if (len == 0) return false;
                if (type == NtsProtocol.FieldUniqueIdentifier)
                {
                    gotUniqueId = Slice(request, offset + 4, len - 4);
                }
                else if (type == NtsProtocol.FieldNtsAuthenticator)
                {
                    authOffset = offset;
                    break;
                }
                offset += len;
            }
            if (authOffset < 0 || (object)gotUniqueId == null) return false;
            if (!SameBytes(gotUniqueId, expectedUniqueId)) return false;

            int bodyStart = authOffset + 4;
            int nonceLen = (request[bodyStart] << 8) | request[bodyStart + 1];
            int cipherLen = (request[bodyStart + 2] << 8) | request[bodyStart + 3];
            int cipherStart = bodyStart + 4 + PadTo4(nonceLen);
            byte[] sealedData = Slice(request, cipherStart, cipherLen);
            byte[] associated = Slice(request, 0, authOffset);
            if (sealedData.Length < 16) return false;
            byte[] plaintext = new byte[sealedData.Length - 16];
            return NtsNative.SivDecrypt(c2sKey, associated, nonce, sealedData, plaintext) >= 0;
        }

        private static byte[] NtpHeader(byte firstByte)
        {
            byte[] header = new byte[48];
            header[0] = firstByte;
            header[1] = 3;
            return header;
        }

        private static byte[] Repeat(int value, int count)
        {
            byte[] data = new byte[count];
            for (int i = 0; i < count; i++) data[i] = (byte)value;
            return data;
        }

        private static byte[] Slice(byte[] source, int offset, int count)
        {
            byte[] result = new byte[count];
            for (int i = 0; i < count; i++) result[i] = source[offset + i];
            return result;
        }

        private static int PadTo4(int n)
        {
            return (n + 3) & ~3;
        }

        private static bool SameBytes(byte[] a, byte[] b)
        {
            if (a.Length != b.Length) return false;
            for (int i = 0; i < a.Length; i++)
            {
                if (a[i] != b[i]) return false;
            }
            return true;
        }
    }
}
