// SPDX-License-Identifier: Apache-2.0
#if UNITY_5_3_OR_NEWER
using UnityEngine;

namespace Aurix.Unity
{
    /// <summary>Scene lookups that compile without deprecation warnings on every supported Unity version.</summary>
    internal static class AurixFind
    {
        public static T First<T>() where T : Object
        {
#if UNITY_2023_1_OR_NEWER
            return Object.FindFirstObjectByType<T>();
#else
            return Object.FindObjectOfType<T>();
#endif
        }
    }
}
#endif
