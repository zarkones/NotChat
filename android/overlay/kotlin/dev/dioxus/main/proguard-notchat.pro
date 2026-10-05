# NotChat release (R8) keep rules. Picked up by the dx-generated app/build.gradle.kts
# (proguardFiles includes every **/*.pro under the app module).
# Rust calls NotChatBridge static methods by name via JNI and Kotlin declares
# `external` natives resolved by symbol name (Java_dev_dioxus_main_NotChatBridge_*),
# so nothing in our package may be renamed or stripped.
-keep class dev.dioxus.main.** { *; }
-keepclasseswithmembernames,includedescriptorclasses class * { native <methods>; }
-keepattributes SourceFile,LineNumberTable
