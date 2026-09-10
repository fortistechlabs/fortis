# fortis release keep rules. R8 full mode; strictFullModeForKeepRules is off
# (android/gradle.properties).

-keepattributes *Annotation*, Signature, InnerClasses, EnclosingMethod, Exceptions

# --- UniFFI / Gobley bindings ---------------------------------------------------
# The generated bindings are a JNA-backed FFI: method and field names must match
# the native symbols in libwallet_ffi.so exactly, and the callback/record classes
# are instantiated reflectively. Keep the lot.
-keep class uniffi.** { *; }
-keep class gobley.** { *; }
-keep class dev.gobley.** { *; }

# --- JNA ----------------------------------------------------------------------
-keep class com.sun.jna.** { *; }
-keep interface com.sun.jna.** { *; }
-keep class * extends com.sun.jna.** { *; }
-keepclassmembers class * extends com.sun.jna.** { *; }
-keepclassmembers class * implements com.sun.jna.** { *; }
-dontwarn java.awt.**

# --- OkHttp / Okio ----------------------------------------------------------
-dontwarn okhttp3.**
-dontwarn okio.**
-dontwarn org.conscrypt.**
-dontwarn org.bouncycastle.**
-dontwarn org.openjsse.**

# --- ZXing embedded scanner --------------------------------------------------
-keep class com.google.zxing.** { *; }
-keep class com.journeyapps.barcodescanner.** { *; }
-dontwarn com.google.zxing.**

# --- Kotlin coroutines ------------------------------------------------------
-dontwarn kotlinx.coroutines.**

# --- DataStore proto -------------------------------------------------------
-keep class androidx.datastore.*.** { *; }
