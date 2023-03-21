import 'package:flutter/material.dart';
import 'package:get_10101/features/wallet/wallet_screen.dart';
import 'package:get_10101/common/settings_screen.dart';
import 'package:get_10101/bridge_generated/bridge_definitions.dart' as bridge;

class WalletSettingsScreen extends StatelessWidget {
  static const route = "${WalletScreen.route}/$subRouteName";
  static const subRouteName = "settings";
  final bridge.Config config;

  const WalletSettingsScreen({super.key, required this.config});

  @override
  Widget build(BuildContext context) {
    return SettingsScreen(fromRoute: route, config: config);
  }
}
