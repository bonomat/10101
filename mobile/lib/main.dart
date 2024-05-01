import 'package:flutter/material.dart';
import 'package:flutter_native_splash/flutter_native_splash.dart';
import 'package:get_10101/backend.dart';
import 'package:get_10101/common/color.dart';
import 'package:get_10101/common/init_service.dart';
import 'package:get_10101/common/routes.dart';
import 'package:get_10101/features/trade/trade_theme.dart';
import 'package:get_10101/features/wallet/wallet_theme.dart';
import 'package:get_10101/util/notifications.dart';
import 'package:go_router/go_router.dart';
import 'package:provider/provider.dart';
import 'package:get_10101/logger/logger.dart';

const Color appBackgroundColor = Color(0xFFFAFAFA);

void main() async {
  WidgetsBinding widgetsBinding = WidgetsFlutterBinding.ensureInitialized();
  FlutterNativeSplash.preserve(widgetsBinding: widgetsBinding);

  await initLogging();
  await initFirebase();
  await setConfig();

  runApp(MultiProvider(providers: createProviders(), child: const TenTenOneApp()));
}

class TenTenOneApp extends StatefulWidget {
  const TenTenOneApp({Key? key}) : super(key: key);

  @override
  State<TenTenOneApp> createState() => _TenTenOneAppState();
}

class _TenTenOneAppState extends State<TenTenOneApp> with WidgetsBindingObserver {
  final GlobalKey<ScaffoldMessengerState> scaffoldMessengerKey =
      GlobalKey<ScaffoldMessengerState>();

  late GoRouter _router;

  @override
  void initState() {
    super.initState();

    WidgetsBinding.instance.addObserver(this);

    _router = createRoutes();

    subscribeToNotifiers(context);
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    logger.d("AppLifecycleState changed to: $state");
  }

  @override
  void dispose() {
    WidgetsBinding.instance.removeObserver(this);
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    MaterialColor swatch = tenTenOnePurple;

    return MaterialApp.router(
      title: "10101",
      scaffoldMessengerKey: scaffoldMessengerKey,
      theme: ThemeData(
        primarySwatch: swatch,
        scaffoldBackgroundColor: appBackgroundColor,
        cardTheme: const CardTheme(
            shape: RoundedRectangleBorder(borderRadius: BorderRadius.all(Radius.circular(12.0))),
            surfaceTintColor: Colors.white,
            color: Colors.white),
        dialogBackgroundColor: Colors.white,
        dialogTheme: const DialogTheme(
          shape: RoundedRectangleBorder(borderRadius: BorderRadius.all(Radius.circular(12.0))),
          backgroundColor: Colors.white,
        ),
        elevatedButtonTheme: ElevatedButtonThemeData(
          style: ButtonStyle(
            // this is the button background color
            backgroundColor: MaterialStateProperty.all<Color>(tenTenOnePurple),
            // this is the button text color
            foregroundColor: MaterialStateProperty.all<Color>(Colors.white),
            shape: MaterialStateProperty.all<RoundedRectangleBorder>(
              RoundedRectangleBorder(
                borderRadius: BorderRadius.circular(5.0),
              ),
            ),
          ),
        ),
        inputDecorationTheme: InputDecorationTheme(
          prefixIconColor: MaterialStateColor.resolveWith(
            (Set<MaterialState> states) {
              if (states.contains(MaterialState.focused)) {
                return tenTenOnePurple;
              }
              if (states.contains(MaterialState.error)) {
                return Colors.red;
              }
              return Colors.grey;
            },
          ),
        ),
        iconTheme: IconThemeData(
          color: tenTenOnePurple.shade800,
          size: 32,
        ),
        extensions: <ThemeExtension<dynamic>>[
          const TradeTheme(),
          WalletTheme(colors: ColorScheme.fromSwatch(primarySwatch: swatch)),
        ],
      ),
      routerConfig: _router,
      debugShowCheckedModeBanner: false,
    );
  }
}
