# kotlinx-serialization
-keepattributes *Annotation*, InnerClasses
-dontnote kotlinx.serialization.AnnotationsKt
-keepclassmembers class kotlinx.serialization.json.** { *** Companion; }
-keepclasseswithmembers class kotlinx.serialization.json.** { kotlinx.serialization.KSerializer serializer(...); }
-keep,includedescriptorclasses class li.rajeshgo.sm.**$$serializer { *; }
-keepclassmembers class li.rajeshgo.sm.** { *** Companion; }
-keepclasseswithmembers class li.rajeshgo.sm.** { kotlinx.serialization.KSerializer serializer(...); }
