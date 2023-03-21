import 'package:flutter/material.dart';
import 'package:get_10101/features/trade/settings_screen.dart';
import 'package:get_10101/features/wallet/settings_screen.dart';
import 'package:get_10101/bridge_generated/bridge_definitions.dart' as bridge;

class SettingsScreen extends StatelessWidget {
  final bridge.Config config;
  const SettingsScreen({required this.fromRoute, super.key, required this.config});

  final String fromRoute;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text("Settings")),
      body: SafeArea(
          child: Column(children: [
        Text(
          "Wallet Settings",
          style: TextStyle(
              fontWeight:
                  fromRoute == WalletSettingsScreen.route ? FontWeight.bold : FontWeight.normal),
        ),
        const Divider(),
        Text("Trade Settings",
            style: TextStyle(
                fontWeight:
                    fromRoute == TradeSettingsScreen.route ? FontWeight.bold : FontWeight.normal)),
        const Divider(),
        const Text("App Info"),
        Table(
          border: TableBorder.symmetric(inside: const BorderSide(width: 1)),
          children: [
            TableRow(
              children: [
                // First column cell
                const Center(
                  child: Text('Electrum'),
                ),
                // Second column cell
                Center(
                  child: SelectableText(config.electrsEndpoint),
                ),
              ],
            ),
            TableRow(
              children: [
                // First column cell
                const Center(
                  child: Text('Network'),
                ),
                // Second column cell
                Center(
                  child: SelectableText(config.network),
                ),
              ],
            ),
            TableRow(
              children: [
                // First column cell
                const Center(
                  child: Text('Coordinator'),
                ),
                // Second column cell
                Center(
                  child: SelectableText(
                      "${config.coordinatorPubkey}@${config.host}:${config.p2PPort}"),
                ),
              ],
            ),
          ],
        ),
      ])),
    );
  }
}
